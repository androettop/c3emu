//! Thumb (16-bit) instruction set, ARMv5T: one small handler per format, chosen when
//! a block is decoded (decode).

use super::arm::{shift_imm, shift_reg};
use super::block::OpFn;
use super::{Cpu, Ex, R};

#[inline(always)]
fn add_with_carry(a: u32, b: u32, cin: bool) -> (u32, bool, bool) {
    let r64 = a as u64 + b as u64 + cin as u64;
    let r = r64 as u32;
    (r, r64 >> 32 != 0, (!(a ^ b) & (a ^ r)) >> 31 != 0)
}

/// Does this Thumb instruction end a block?
pub(super) fn ends_block(i: u16) -> bool {
    let i = i as u32;
    matches!(i >> 8, 0xDE | 0xDF) // SWI / undefined (a conditional branch is a side exit)
        || matches!(i >> 11, 0x1C | 0x1D | 0x1F) // B, BLX / BL suffix (not the BL prefix)
        || i & 0xFF00 == 0x4700 // BX / BLX
        || i & 0xFF87 == 0x4487 || i & 0xFF87 == 0x4687 // ADD / MOV pc
        || i & 0xFF00 == 0xBD00 // POP {.., pc}
        || i & 0xFF00 == 0xBE00 // BKPT
}

/// The handler for instruction `i`.
pub(super) fn decode(i: u16) -> OpFn {
    let i = i as u32;
    match i >> 11 {
        0x00..=0x02 => t_shift,
        0x03 => t_addsub,
        0x04 => t_movi,
        0x05 => t_cmpi,
        0x06 => t_addi,
        0x07 => t_subi,
        0x08 => {
            if i & 0x0400 == 0 { t_alu } else { t_hireg }
        }
        0x09 => t_ldr_pc,
        0x0A | 0x0B => t_ldst_reg,
        0x0C => t_str_imm,
        0x0D => t_ldr_imm,
        0x0E => t_strb_imm,
        0x0F => t_ldrb_imm,
        0x10 => t_strh_imm,
        0x11 => t_ldrh_imm,
        0x12 => t_str_sp,
        0x13 => t_ldr_sp,
        0x14 | 0x15 => t_add_pcsp,
        0x16 | 0x17 => match (i >> 8) & 0xF {
            0 => t_add_sp,
            4 | 5 => t_push,
            0xC | 0xD => t_pop,
            0xE => t_bkpt,
            _ => t_undef,
        },
        0x18 => t_stmia,
        0x19 => t_ldmia,
        0x1A | 0x1B => match (i >> 8) & 0xF {
            0xE => t_undef,
            0xF => t_swi,
            _ => t_bcond,
        },
        0x1C => t_b,
        0x1D => t_blx_suffix,
        0x1E => t_bl_prefix,
        _ => t_bl_suffix,
    }
}

fn t_shift(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let (r, cf) = shift_imm(c.regs[((i >> 3) & 7) as usize], (i >> 11) & 3, (i >> 6) & 31, c.flag_c());
    c.regs[(i & 7) as usize] = r;
    c.set_nzc(r, cf);
    Ok(())
}

fn t_addsub(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let b = if i & (1 << 10) != 0 { (i >> 6) & 7 } else { c.regs[((i >> 6) & 7) as usize] };
    let a = c.regs[((i >> 3) & 7) as usize];
    let (r, cf, v) = if i & (1 << 9) != 0 { add_with_carry(a, !b, true) } else { add_with_carry(a, b, false) };
    c.regs[(i & 7) as usize] = r;
    c.set_nzcv(r, cf, v);
    Ok(())
}

fn t_movi(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let v = i & 0xFF;
    c.regs[((i >> 8) & 7) as usize] = v;
    c.set_nz(v);
    Ok(())
}

fn t_cmpi(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let (r, cf, v) = add_with_carry(c.regs[((i >> 8) & 7) as usize], !(i & 0xFF), true);
    c.set_nzcv(r, cf, v);
    Ok(())
}

fn t_addi(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let rd = ((i >> 8) & 7) as usize;
    let (r, cf, v) = add_with_carry(c.regs[rd], i & 0xFF, false);
    c.regs[rd] = r;
    c.set_nzcv(r, cf, v);
    Ok(())
}

fn t_subi(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let rd = ((i >> 8) & 7) as usize;
    let (r, cf, v) = add_with_carry(c.regs[rd], !(i & 0xFF), true);
    c.regs[rd] = r;
    c.set_nzcv(r, cf, v);
    Ok(())
}

fn t_ldr_pc(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let a = (c.pc.wrapping_add(4) & !3).wrapping_add((i & 0xFF) << 2);
    let v = c.ld32(a)?;
    c.regs[((i >> 8) & 7) as usize] = v;
    Ok(())
}

fn t_ldst_reg(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let rd = (i & 7) as usize;
    let a = c.regs[((i >> 3) & 7) as usize].wrapping_add(c.regs[((i >> 6) & 7) as usize]);
    match (i >> 9) & 7 {
        0 => c.st32(a, c.regs[rd])?,
        1 => c.st16(a, c.regs[rd])?,
        2 => c.st8(a, c.regs[rd])?,
        3 => c.regs[rd] = c.ld8(a)? as u8 as i8 as i32 as u32,
        4 => c.regs[rd] = c.ld32(a)?,
        5 => c.regs[rd] = c.ld16(a)?,
        6 => c.regs[rd] = c.ld8(a)?,
        _ => c.regs[rd] = c.ld16(a)? as u16 as i16 as i32 as u32,
    }
    Ok(())
}

#[inline(always)]
fn imm5_addr(c: &Cpu<'_>, i: u32, scale: u32) -> u32 {
    c.regs[((i >> 3) & 7) as usize].wrapping_add(((i >> 6) & 31) << scale)
}

fn t_str_imm(c: &mut Cpu<'_>, i: u32) -> R<()> {
    c.st32(imm5_addr(c, i, 2), c.regs[(i & 7) as usize])
}

fn t_ldr_imm(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let v = c.ld32(imm5_addr(c, i, 2))?;
    c.regs[(i & 7) as usize] = v;
    Ok(())
}

fn t_strb_imm(c: &mut Cpu<'_>, i: u32) -> R<()> {
    c.st8(imm5_addr(c, i, 0), c.regs[(i & 7) as usize])
}

fn t_ldrb_imm(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let v = c.ld8(imm5_addr(c, i, 0))?;
    c.regs[(i & 7) as usize] = v;
    Ok(())
}

fn t_strh_imm(c: &mut Cpu<'_>, i: u32) -> R<()> {
    c.st16(imm5_addr(c, i, 1), c.regs[(i & 7) as usize])
}

fn t_ldrh_imm(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let v = c.ld16(imm5_addr(c, i, 1))?;
    c.regs[(i & 7) as usize] = v;
    Ok(())
}

fn t_str_sp(c: &mut Cpu<'_>, i: u32) -> R<()> {
    c.st32(c.regs[13].wrapping_add((i & 0xFF) << 2), c.regs[((i >> 8) & 7) as usize])
}

fn t_ldr_sp(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let v = c.ld32(c.regs[13].wrapping_add((i & 0xFF) << 2))?;
    c.regs[((i >> 8) & 7) as usize] = v;
    Ok(())
}

fn t_add_pcsp(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let base = if i & (1 << 11) != 0 { c.regs[13] } else { c.pc.wrapping_add(4) & !3 };
    c.regs[((i >> 8) & 7) as usize] = base.wrapping_add((i & 0xFF) << 2);
    Ok(())
}

fn t_add_sp(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let off = (i & 0x7F) << 2;
    c.regs[13] = if i & 0x80 != 0 { c.regs[13].wrapping_sub(off) } else { c.regs[13].wrapping_add(off) };
    Ok(())
}

fn t_push(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let list = i & 0xFF;
    let lr = i & 0x100 != 0;
    let n = list.count_ones() + lr as u32;
    let start = c.regs[13].wrapping_sub(4 * n);
    let mut a = start;
    for r in 0..8 {
        if list & (1 << r) != 0 {
            c.st32(a, c.regs[r])?;
            a = a.wrapping_add(4);
        }
    }
    if lr {
        c.st32(a, c.regs[14])?;
    }
    c.regs[13] = start;
    Ok(())
}

fn t_pop(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let list = i & 0xFF;
    let mut a = c.regs[13];
    let mut vals = [0u32; 8];
    for (r, v) in vals.iter_mut().enumerate() {
        if list & (1 << r) != 0 {
            *v = c.ld32(a)?;
            a = a.wrapping_add(4);
        }
    }
    let pc = if i & 0x100 != 0 {
        let v = c.ld32(a)?;
        a = a.wrapping_add(4);
        Some(v)
    } else {
        None
    };
    for (r, v) in vals.iter().enumerate() {
        if list & (1 << r) != 0 {
            c.regs[r] = *v;
        }
    }
    c.regs[13] = a;
    if let Some(v) = pc {
        c.bx(v); // ARMv5: interworking
    }
    Ok(())
}

fn t_bkpt(_c: &mut Cpu<'_>, _i: u32) -> R<()> {
    Err(Ex::Bkpt)
}

fn t_undef(_c: &mut Cpu<'_>, _i: u32) -> R<()> {
    Err(Ex::Undef)
}

fn t_swi(_c: &mut Cpu<'_>, _i: u32) -> R<()> {
    Err(Ex::Swi)
}

fn t_stmia(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let rb = ((i >> 8) & 7) as usize;
    let list = i & 0xFF;
    let mut a = c.regs[rb];
    for r in 0..8 {
        if list & (1 << r) != 0 {
            c.st32(a, c.regs[r])?;
            a = a.wrapping_add(4);
        }
    }
    c.regs[rb] = a;
    Ok(())
}

fn t_ldmia(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let rb = ((i >> 8) & 7) as usize;
    let list = i & 0xFF;
    let mut a = c.regs[rb];
    let mut vals = [0u32; 8];
    for (r, v) in vals.iter_mut().enumerate() {
        if list & (1 << r) != 0 {
            *v = c.ld32(a)?;
            a = a.wrapping_add(4);
        }
    }
    c.regs[rb] = a;
    for (r, v) in vals.iter().enumerate() {
        if list & (1 << r) != 0 {
            c.regs[r] = *v; // the loaded base wins
        }
    }
    Ok(())
}

fn t_bcond(c: &mut Cpu<'_>, i: u32) -> R<()> {
    if c.cond((i >> 8) & 0xF) {
        let off = ((i & 0xFF) as i8 as i32 as u32) << 1;
        c.jump(c.pc.wrapping_add(4).wrapping_add(off));
    }
    Ok(())
}

fn t_b(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let off = ((((i & 0x7FF) << 21) as i32) >> 20) as u32;
    c.jump(c.pc.wrapping_add(4).wrapping_add(off));
    Ok(())
}

fn t_blx_suffix(c: &mut Cpu<'_>, i: u32) -> R<()> {
    if i & 1 != 0 {
        return Err(Ex::Undef);
    }
    let t = c.regs[14].wrapping_add((i & 0x7FF) << 1) & !3;
    c.regs[14] = c.pc.wrapping_add(2) | 1;
    c.thumb = false;
    c.jump(t);
    Ok(())
}

fn t_bl_prefix(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let off = ((((i & 0x7FF) << 21) as i32) >> 9) as u32;
    c.regs[14] = c.pc.wrapping_add(4).wrapping_add(off);
    Ok(())
}

fn t_bl_suffix(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let t = c.regs[14].wrapping_add((i & 0x7FF) << 1);
    c.regs[14] = c.pc.wrapping_add(2) | 1;
    c.jump(t);
    Ok(())
}

fn t_alu(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let rd = (i & 7) as usize;
    let rs = ((i >> 3) & 7) as usize;
    let (a, b) = (c.regs[rd], c.regs[rs]);
    let cf = c.flag_c();
    match (i >> 6) & 0xF {
        0x0 => {
            let r = a & b;
            c.regs[rd] = r;
            c.set_nz(r);
        }
        0x1 => {
            let r = a ^ b;
            c.regs[rd] = r;
            c.set_nz(r);
        }
        op @ (0x2 | 0x3 | 0x4 | 0x7) => {
            let ty = match op {
                0x2 => 0,
                0x3 => 1,
                0x4 => 2,
                _ => 3,
            };
            let (r, co) = shift_reg(a, ty, b, cf);
            c.regs[rd] = r;
            c.set_nzc(r, co);
        }
        0x5 => {
            let (r, co, v) = add_with_carry(a, b, cf);
            c.regs[rd] = r;
            c.set_nzcv(r, co, v);
        }
        0x6 => {
            let (r, co, v) = add_with_carry(a, !b, cf);
            c.regs[rd] = r;
            c.set_nzcv(r, co, v);
        }
        0x8 => c.set_nz(a & b),
        0x9 => {
            let (r, co, v) = add_with_carry(0, !b, true);
            c.regs[rd] = r;
            c.set_nzcv(r, co, v);
        }
        0xA => {
            let (r, co, v) = add_with_carry(a, !b, true);
            c.set_nzcv(r, co, v);
        }
        0xB => {
            let (r, co, v) = add_with_carry(a, b, false);
            c.set_nzcv(r, co, v);
        }
        0xC => {
            let r = a | b;
            c.regs[rd] = r;
            c.set_nz(r);
        }
        0xD => {
            let r = a.wrapping_mul(b);
            c.regs[rd] = r;
            c.set_nz(r);
        }
        0xE => {
            let r = a & !b;
            c.regs[rd] = r;
            c.set_nz(r);
        }
        _ => {
            let r = !b;
            c.regs[rd] = r;
            c.set_nz(r);
        }
    }
    Ok(())
}

fn t_hireg(c: &mut Cpu<'_>, i: u32) -> R<()> {
    let rd = ((i & 7) | ((i >> 4) & 8)) as usize;
    let rm = ((i >> 3) & 0xF) as usize;
    match (i >> 8) & 3 {
        0 => {
            let r = c.r(rd).wrapping_add(c.r(rm));
            c.w(rd, r);
        }
        1 => {
            let (r, co, v) = add_with_carry(c.r(rd), !c.r(rm), true);
            c.set_nzcv(r, co, v);
        }
        2 => {
            let v = c.r(rm);
            c.w(rd, v);
        }
        _ => {
            let t = c.r(rm);
            if i & 0x80 != 0 {
                c.regs[14] = c.pc.wrapping_add(2) | 1; // BLX
            }
            c.bx(t);
        }
    }
    Ok(())
}
