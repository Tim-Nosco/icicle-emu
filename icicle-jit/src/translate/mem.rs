//! Module for interacting with memory inside of the JIT.

use std::convert::TryInto;

use cranelift::prelude::{types, Block, InstBuilder, IntCC, MemFlags, Type, Value};
use cranelift_codegen::ir::Endianness;
use icicle_cpu::mem::{
    self, perm,
    physical::{PageData, OFFSET_BITS},
    tlb::{TLBEntry, TLB_INDEX_BITS},
};
use memoffset::offset_of;

use crate::translate::{is_native_size, sized_int, Translator};

#[derive(Clone, Copy)]
enum AccessKind {
    Load,
    Store,
}

impl AccessKind {
    fn perm(&self) -> u8 {
        match self {
            Self::Load => perm::READ | perm::INIT | perm::MAP,
            Self::Store => perm::WRITE | perm::MAP,
        }
    }

    fn tlb_offset(&self) -> i32 {
        match self {
            Self::Load => offset_of!(mem::tlb::TranslationCache, read).try_into().unwrap(),
            Self::Store => offset_of!(mem::tlb::TranslationCache, write).try_into().unwrap(),
        }
    }
}

/// Generate code for checking that `addr` is aligned to at least `bytes`
fn check_alignment(trans: &mut Translator, addr: Value, bytes: u8, unaligned_block: Block) {
    if bytes == 1 {
        // We know statically that the address is aligned.
        return;
    }

    let cond = trans.builder.ins().band_imm(addr, (bytes - 1) as i64);
    trans.builder.ins().brnz(cond, unaligned_block, &[]);
    trans.jump_next_block();
}

/// Generate code for modifying `value` with a `>>` of `shift` bits followed by an `&` with `mask`.
fn rshift_and_mask(trans: &mut Translator, value: Value, shift: i64, mask: i64) -> Value {
    let tmp = trans.builder.ins().ushr_imm(value, shift);
    trans.builder.ins().band_imm(tmp, mask)
}

/// Generate code for modifying `value` with a `<<` of `shift` bits followed by an `&` with `mask`.
fn lshift_and_mask(trans: &mut Translator, value: Value, shift: i64, mask: i64) -> Value {
    let tmp = trans.builder.ins().ishl_imm(value, shift);
    trans.builder.ins().band_imm(tmp, mask)
}

/// Generate code for looking up an entry in the TLB and comparing the tag.
fn tlb_lookup(trans: &mut Translator, addr: Value, kind: AccessKind, not_found: Block) -> Value {
    // TLB reads do not alias with loads to actual memory.
    let mem_flags = MemFlags::trusted().with_vmctx();

    // Find the host address that contains the TLB entry for this address.
    let tlb_entry_size: i64 = std::mem::size_of::<TLBEntry>().try_into().unwrap();
    assert_eq!(tlb_entry_size.count_ones(), 1);

    let tlb_entry_size_bits = tlb_entry_size.trailing_zeros() as usize;
    let index_shift = (OFFSET_BITS - tlb_entry_size_bits) as i64;
    let index_mask = (((1_u64 << TLB_INDEX_BITS) - 1) as i64) << tlb_entry_size_bits;

    let entry_offset = rshift_and_mask(trans, addr, index_shift, index_mask);
    let tlb_addr = trans.builder.ins().iadd(trans.tlb_ptr, entry_offset);

    // Load the tag
    let kind_offset = kind.tlb_offset();
    let expected_tag = trans.builder.ins().load(types::I64, mem_flags, tlb_addr, kind_offset + 0);

    // Check that the tag matches
    let tag_shift = (OFFSET_BITS + TLB_INDEX_BITS) as i64;
    let tag_mask = ((1_u64 << mem::tlb::TLB_TAG_BITS) - 1) as i64;
    let tag = rshift_and_mask(trans, addr, tag_shift, tag_mask);
    trans.builder.ins().br_icmp(IntCC::NotEqual, tag, expected_tag, not_found, &[]);
    trans.jump_next_block();

    // Load the pointer
    trans.builder.ins().load(types::I64, mem_flags, tlb_addr, kind_offset + 8)
}

/// Generate code for looking up an entry in the TLB and comparing the tag if the address is known
/// statically.
fn tlb_lookup_const(
    trans: &mut Translator,
    addr: u64,
    kind: AccessKind,
    not_found: Block,
) -> Value {
    // TLB reads do not alias with loads to actual memory.
    let mem_flags = MemFlags::trusted().with_vmctx();

    // Find the host address that contains the TLB entry for this address.
    let tlb_entry_size: i64 = std::mem::size_of::<TLBEntry>().try_into().unwrap();
    assert_eq!(tlb_entry_size.count_ones(), 1);

    let index = icicle_cpu::mem::tlb::TranslationCache::index(addr) as i64;
    let entry_offset = trans.builder.ins().iconst(types::I64, index * tlb_entry_size);
    let tlb_addr = trans.builder.ins().iadd(trans.tlb_ptr, entry_offset);

    // Load the tag
    let kind_offset = kind.tlb_offset();
    let expected_tag = trans.builder.ins().load(types::I64, mem_flags, tlb_addr, kind_offset + 0);

    // Check that the tag matches
    let tag =
        trans.builder.ins().iconst(types::I64, icicle_cpu::mem::tlb::TLBEntry::tag(addr) as i64);
    trans.builder.ins().br_icmp(IntCC::NotEqual, tag, expected_tag, not_found, &[]);
    trans.jump_next_block();

    // Load the pointer
    trans.builder.ins().load(types::I64, mem_flags, tlb_addr, kind_offset + 8)
}

/// Generate code for checking that the permissons associated with the value at `host_addr`
/// satisfies `perm`.
fn check_perm(trans: &mut Translator, host_addr: Value, size: u8, perm: u8, invalid_perm: Block) {
    let perm_offset: i32 = offset_of!(PageData, perm).try_into().unwrap();

    let ty = sized_int(size);
    let value =
        trans.builder.ins().load(ty, MemFlags::trusted().with_heap(), host_addr, perm_offset);

    // Duplicate `perm` to cover all bytes that we need to check
    let perm = splat_const(trans, perm, ty);

    // Check if the all the bits in `perm` are set for this address.
    let value = trans.builder.ins().band(value, perm);
    trans.builder.ins().br_icmp(IntCC::NotEqual, value, perm, invalid_perm, &[]);
    trans.jump_next_block();
}

/// Create a constant of `ty` that consists of `value` repeated for every byte.
fn splat_const(trans: &mut Translator, value: u8, ty: Type) -> Value {
    let mut tmp = [0; 8];
    for i in 0..ty.bytes().min(8) {
        tmp[i as usize] = value;
    }
    let expanded = i64::from_le_bytes(tmp);

    match ty {
        types::I8 | types::I16 | types::I32 | types::I64 => {
            trans.builder.ins().iconst(ty, expanded)
        }
        types::I128 => {
            let lo = trans.builder.ins().iconst(types::I64, expanded);
            let hi = trans.builder.ins().iconst(types::I64, expanded);
            trans.builder.ins().iconcat(lo, hi)
        }
        _ => unreachable!(),
    }
}

pub(super) fn load_host(trans: &mut Translator, addr: Value, size: u8) -> Value {
    let ty = sized_int(size);

    let mut flags = MemFlags::trusted().with_heap();
    flags.set_endianness(trans.ctx.endianness);
    let mut result = trans.builder.ins().load(ty, flags, addr, 0);

    // Setting the endianness doesn't actually do anything in x86_64 backend for cranelift
    // currently, so we manually perform a byte swap operation.
    if trans.ctx.endianness != Endianness::Little {
        result = bswap(trans, result, ty);
    }

    result
}

/// Generate code for loading a value from RAM.
pub(super) fn load_ram(trans: &mut Translator, guest_addr: pcode::Value, output: pcode::VarNode) {
    let size = output.size;
    if !is_native_size(size) || trans.ctx.disable_jit_mem {
        trans.interpret(pcode::Instruction::from((
            output,
            pcode::Op::Load(0),
            pcode::Inputs::one(guest_addr),
        )));
        // Check for memory exceptions.
        trans.maybe_exit_jit();
        return;
    }

    let guest_addr_val = trans.read_zxt(guest_addr, 8);

    if let pcode::Value::Const(addr, _) = guest_addr {
        if !is_aligned(addr, size) {
            let value = load_fallback(trans, output, guest_addr_val);
            trans.write(output, value);
            return;
        }
    }

    let success_block = trans.builder.create_block();
    trans.builder.append_block_param(success_block, sized_int(size));

    let fallback_block = trans.builder.create_block();
    trans.builder.set_cold_block(fallback_block);

    let host_addr = try_inline_access(
        trans,
        guest_addr,
        guest_addr_val,
        size,
        AccessKind::Load,
        fallback_block,
    );

    // inline access (fallthrough):
    {
        let value = load_host(trans, host_addr, size);
        trans.builder.ins().jump(success_block, &[value]);
    }

    // fallback:
    {
        trans.builder.switch_to_block(fallback_block);
        trans.builder.seal_block(fallback_block);
        let value = load_fallback(trans, output, guest_addr_val);
        trans.builder.ins().jump(success_block, &[value]);
    }

    // success:
    trans.builder.switch_to_block(success_block);
    trans.builder.seal_block(success_block);

    let value = trans.builder.block_params(success_block)[0];
    trans.write(output, value);
}

fn load_fallback(trans: &mut Translator, output: pcode::VarNode, guest_addr: Value) -> Value {
    // Flush PC to memory to allow any memory hooks to have the correct value.
    trans.flush_current_pc();

    let func = trans.symbols.mmu.load(output.size);
    let args = [trans.vm_ptr.0, guest_addr];
    let call = trans.builder.ins().call(func, &args);
    let value = match trans.builder.inst_results(call) {
        &[result] => result,
        _ => unreachable!(),
    };
    let block = trans.maybe_exit_jit();
    trans.builder.set_cold_block(block);
    value
}

pub(super) fn store_host(trans: &mut Translator, addr: Value, mut value: Value, size: u8) {
    let ty = sized_int(size);
    let mut flags = MemFlags::trusted().with_heap();
    flags.set_endianness(trans.ctx.endianness);
    // Setting the endianness doesn't actually do anything in x86_64 backend for cranelift
    // currently, so we manually perform a byte swap operation.
    if trans.ctx.endianness != Endianness::Little {
        value = bswap(trans, value, ty);
    }
    trans.builder.ins().store(flags, value, addr, 0);
}

/// Generate code for storing a value to RAM.
pub(super) fn store_ram(trans: &mut Translator, guest_addr: pcode::Value, value: pcode::Value) {
    let size = value.size();
    if !is_native_size(size) || trans.ctx.disable_jit_mem {
        trans.interpret(pcode::Instruction::from((
            pcode::Op::Store(0),
            pcode::Inputs::new(guest_addr, value),
        )));
        // Check for memory exceptions.
        trans.maybe_exit_jit();
        return;
    }

    let guest_addr_val = trans.read_zxt(guest_addr, 8);
    let value = trans.read_int(value);

    if let pcode::Value::Const(addr, _) = guest_addr {
        if !is_aligned(addr, size) {
            store_fallback(trans, size, guest_addr_val, value);
            return;
        }
    }

    let success_block = trans.builder.create_block();
    let fallback_block = trans.builder.create_block();
    trans.builder.set_cold_block(fallback_block);

    let host_addr = try_inline_access(
        trans,
        guest_addr,
        guest_addr_val,
        size,
        AccessKind::Store,
        fallback_block,
    );

    // inline access (fallthrough):
    {
        store_host(trans, host_addr, value, size);
        trans.builder.ins().jump(success_block, &[]);
    }

    // fallback:
    {
        trans.builder.switch_to_block(fallback_block);
        trans.builder.seal_block(fallback_block);
        store_fallback(trans, size, guest_addr_val, value);
        trans.builder.ins().jump(success_block, &[]);
    }

    // success:
    trans.builder.switch_to_block(success_block);
    trans.builder.seal_block(success_block);
}

/// Handle complex stores using a function provided by the runtime.
fn store_fallback(trans: &mut Translator, size: u8, guest_addr: Value, value: Value) {
    // Flush PC to memory to allow any memory hooks to have the correct value.
    trans.flush_current_pc();

    let func = trans.symbols.mmu.store(size);
    let args = [trans.vm_ptr.0, guest_addr, value];
    trans.builder.ins().call(func, &args);
    let block = trans.maybe_exit_jit();
    trans.builder.set_cold_block(block);
}

fn try_inline_access(
    trans: &mut Translator,
    guest_addr: pcode::Value,
    guest_addr_val: Value,
    size: u8,
    kind: AccessKind,
    fallback_block: Block,
) -> Value {
    if let pcode::Value::Const(guest_addr, _) = guest_addr {
        return try_inline_access_const(trans, guest_addr, size, kind, fallback_block);
    }

    if size != 1 {
        // The JIT currently only supports fully aligned loads/stores, so check that here. This also
        // ensures that any loads/stores will not cross page boundaries.
        //
        // @todo: consider supporting unaligned loads/stores that do not cross page boundaries.
        check_alignment(trans, guest_addr_val, size, fallback_block);
    }

    // Translate the guest address to a host address.
    let page_ptr = tlb_lookup(trans, guest_addr_val, kind, fallback_block);
    let offset = trans.builder.ins().band_imm(guest_addr_val, ((1_u64 << OFFSET_BITS) - 1) as i64);
    let host_addr = trans.builder.ins().iadd(page_ptr, offset);

    check_perm(trans, host_addr, size, kind.perm(), fallback_block);

    host_addr
}

fn try_inline_access_const(
    trans: &mut Translator,
    guest_addr: u64,
    size: u8,
    kind: AccessKind,
    fallback_block: Block,
) -> Value {
    let page_ptr = tlb_lookup_const(trans, guest_addr, kind, fallback_block);
    let offset =
        trans.builder.ins().iconst(types::I64, (guest_addr & (1_u64 << OFFSET_BITS) - 1) as i64);
    let host_addr = trans.builder.ins().iadd(page_ptr, offset);

    check_perm(trans, host_addr, size, kind.perm(), fallback_block);

    host_addr
}

/// Swaps the byteorder of `input`
// @todo: replace with native cranelift implementation when avaliable: https://github.com/bytecodealliance/wasmtime/issues/1092
fn bswap(trans: &mut Translator, input: Value, ty: types::Type) -> Value {
    match ty {
        types::I8 => input,
        types::I16 => {
            let a = rshift_and_mask(trans, input, 8, 0x00ff);
            let b = lshift_and_mask(trans, input, 8, 0xff00);
            trans.builder.ins().bor(a, b)
        }
        types::I32 => {
            let a = rshift_and_mask(trans, input, 24, 0x000000ff);
            let b = rshift_and_mask(trans, input, 8, 0x0000ff00);
            let c = lshift_and_mask(trans, input, 8, 0x00ff0000);
            let d = lshift_and_mask(trans, input, 24, 0xff000000);
            let ab = trans.builder.ins().bor(a, b);
            let cd = trans.builder.ins().bor(c, d);
            trans.builder.ins().bor(ab, cd)
        }
        types::I64 => {
            let a = rshift_and_mask(trans, input, 56, 0x00000000_000000ff);
            let b = rshift_and_mask(trans, input, 40, 0x00000000_0000ff00);
            let c = rshift_and_mask(trans, input, 24, 0x00000000_00ff0000);
            let d = rshift_and_mask(trans, input, 8, 0x00000000_ff000000);

            let e = rshift_and_mask(trans, input, 8, 0x000000ff_00000000);
            let f = rshift_and_mask(trans, input, 24, 0x0000ff00_00000000);
            let g = rshift_and_mask(trans, input, 40, 0x00ff0000_00000000);
            let h = rshift_and_mask(trans, input, 56, 0xff000000_00000000_u64 as i64);

            let ab = trans.builder.ins().bor(a, b);
            let cd = trans.builder.ins().bor(c, d);
            let abcd = trans.builder.ins().bor(ab, cd);

            let ef = trans.builder.ins().bor(e, f);
            let gh = trans.builder.ins().bor(g, h);
            let efgh = trans.builder.ins().bor(ef, gh);

            trans.builder.ins().bor(abcd, efgh)
        }
        _ => unimplemented!(),
    }
}

fn is_aligned(value: u64, size: u8) -> bool {
    value & (size as u64 - 1) == 0
}
