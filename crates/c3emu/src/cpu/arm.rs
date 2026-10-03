//! ARM (32-bit) instruction set, ARMv5TE.

use super::block::OpFn;
use super::{Cpu, Ex, R};

#[inline(always)]
fn add_with_carry(a: u32, b: u32, cin: bool) -> (u32, bool, bool) {
    let r64 = a as u64 + b as u64 + cin as u64;
    let r = r64 as u32;
    (r, r64 >> 32 != 0, (!(a ^ b) & (a ^ r)) >> 31 != 0)
}

#[inline(always)]
fn sat_add(a: i32, b: i32) -> (u32, bool) {
    match a.checked_add(b) {
        Some(r) => (r as u32, false),
        None => (if a < 0 { i32::MIN } else { i32::MAX } as u32, true),
    }
}

#[inline(always)]
fn sat_sub(a: i32, b: i32) -> (u32, bool) {
    match a.checked_sub(b) {
        Some(r) => (r as u32, false),
        None => (if a < 0 { i32::MIN } else { i32::MAX } as u32, true),
    }
}

/// Shift by an immediate amount (operand 2 / addressing): (value, carry out).
#[inline(always)]
pub(super) fn shift_imm(v: u32, ty: u32, amt: u32, c: bool) -> (u32, bool) {
    match ty {
        0 => {
            if amt == 0 { (v, c) } else { (v << amt, (v >> (32 - amt)) & 1 != 0) }
        }
        1 => {
            if amt == 0 { (0, v >> 31 != 0) } else { (v >> amt, (v >> (amt - 1)) & 1 != 0) }
        }
        2 => {
            if amt == 0 {
                (((v as i32) >> 31) as u32, v >> 31 != 0)
            } else {
                (((v as i32) >> amt) as u32, (v >> (amt - 1)) & 1 != 0)
            }
        }
        _ => {
            if amt == 0 {
                (((c as u32) << 31) | (v >> 1), v & 1 != 0) // RRX
            } else {
                (v.rotate_right(amt), (v >> (amt - 1)) & 1 != 0)
            }
        }
    }
}

/// Shift by a register amount (bottom byte).
#[inline(always)]
pub(super) fn shift_reg(v: u32, ty: u32, amt: u32, c: bool) -> (u32, bool) {
    let amt = amt & 0xFF;
    if amt == 0 {
        return (v, c);
    }
    match ty {
        0 => match amt {
            1..=31 => (v << amt, (v >> (32 - amt)) & 1 != 0),
            32 => (0, v & 1 != 0),
            _ => (0, false),
        },
        1 => match amt {
            1..=31 => (v >> amt, (v >> (amt - 1)) & 1 != 0),
            32 => (0, v >> 31 != 0),
            _ => (0, false),
        },
        2 => {
            if amt >= 32 {
                (((v as i32) >> 31) as u32, v >> 31 != 0)
            } else {
                (((v as i32) >> amt) as u32, (v >> (amt - 1)) & 1 != 0)
            }
        }
        _ => {
            let r = amt & 31;
            if r == 0 { (v, v >> 31 != 0) } else { (v.rotate_right(r), (v >> (r - 1)) & 1 != 0) }
        }
    }
}

/// Does this ARM instruction end a translation block (changes the flow)?
pub(super) fn ends_block(i: u32) -> bool {
    let op = (i >> 25) & 7;
    let rd15 = (i >> 12) & 0xF == 15;
    i >> 28 == 0xF
        || op == 5 && (i >> 28 == 0xE || i & (1 << 24) != 0) // B (not conditional: side exit), BL
        || i & 0x0FFF_FFD0 == 0x012F_FF10 // BX / BLX reg
        || (op <= 1 && rd15 && (i >> 21) & 0xC != 0x8) // ALU writing pc
        || (op == 2 || op == 3) && i & (1 << 20) != 0 && rd15
        || op == 4 && i & (1 << 20) != 0 && i & 0x8000 != 0
        || op == 7
}

/// Condition check at the top of an ARM handler.
macro_rules! cond {
    ($c:ident, $i:ident) => {
        if $i >> 28 != 0xE && !$c.cond($i >> 28) {
            return Ok(());
        }
    };
}

/// The handler for ARM instruction `i`.
pub(super) fn decode(i: u32) -> OpFn {
    if i >> 28 == 0xF {
        return a_uncond;
    }
    match (i >> 25) & 7 {
        0 => {
            if i & 0x90 == 0x90 {
                a_mul_extra
            } else if i & 0x0190_0000 == 0x0100_0000 {
                a_misc
            } else if i & 0x10 != 0 {
                a_dp_regshift
            } else {
                a_dp_reg
            }
        }
        1 => {
            if i & 0x0190_0000 == 0x0100_0000 {
                if i & 0x0020_0000 != 0 { a_msr_imm } else { a_undef }
            } else {
                a_dp_imm
            }
        }
        2 => a_ldst_imm,
        3 => {
            if i & 0x10 != 0 { a_undef } else { a_ldst_reg }
        }
        4 => a_ldm_stm,
        5 => {
            if i & (1 << 24) != 0 { a_bl } else { a_b }
        }
        6 => a_undef, // LDC/STC/MCRR/MRRC: no such coprocessor
        _ => {
            if i & (1 << 24) != 0 {
                a_swi
            } else if i & 0x10 == 0 {
                a_undef // CDP
            } else {
                a_mcr_mrc
            }
        }
    }
}

fn a_uncond(c: &mut Cpu<'_>, i: u32) -> R<()> {
    c.arm_uncond(i)
}

fn a_mul_extra(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    c.arm_mul_or_extra(i)
}

fn a_misc(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    c.arm_misc(i)
}

fn a_dp_regshift(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    let rs = c.regs[((i >> 8) & 0xF) as usize];
    let rm = (i & 0xF) as usize;
    let v = if rm == 15 { c.pc.wrapping_add(12) } else { c.regs[rm] };
    let (op2, sc) = shift_reg(v, (i >> 5) & 3, rs, c.flag_c());
    c.data_proc(i, op2, sc, (i >> 16) & 0xF == 15)
}

fn a_dp_reg(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    let (op2, sc) = shift_imm(c.r((i & 0xF) as usize), (i >> 5) & 3, (i >> 7) & 31, c.flag_c());
    c.data_proc(i, op2, sc, false)
}

fn a_dp_imm(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    let rot = ((i >> 8) & 0xF) * 2;
    let v = (i & 0xFF).rotate_right(rot);
    let sc = if rot == 0 { c.flag_c() } else { v >> 31 != 0 };
    c.data_proc(i, v, sc, false)
}

fn a_msr_imm(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    c.arm_msr(i, (i & 0xFF).rotate_right(((i >> 8) & 0xF) * 2))
}

fn a_undef(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    Err(Ex::Undef)
}

fn a_ldst_imm(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    c.arm_ldr_str(i, i & 0xFFF)
}

fn a_ldst_reg(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    let (off, _) = shift_imm(c.r((i & 0xF) as usize), (i >> 5) & 3, (i >> 7) & 31, c.flag_c());
    c.arm_ldr_str(i, off)
}

fn a_ldm_stm(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    c.arm_ldm_stm(i)
}

fn a_b(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    let off = ((i << 8) as i32 >> 6) as u32;
    c.jump(c.pc.wrapping_add(8).wrapping_add(off));
    Ok(())
}

fn a_bl(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    let off = ((i << 8) as i32 >> 6) as u32;
    c.regs[14] = c.pc.wrapping_add(4);
    c.jump(c.pc.wrapping_add(8).wrapping_add(off));
    Ok(())
}

fn a_swi(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    Err(Ex::Swi)
}

fn a_mcr_mrc(c: &mut Cpu<'_>, i: u32) -> R<()> {
    cond!(c, i);
    c.arm_mcr_mrc(i)
}

impl<'a> Cpu<'a> {
    pub(super) fn arm_uncond(&mut self, i: u32) -> R<()> {
        if i & 0x0E00_0000 == 0x0A00_0000 {
            // BLX imm
            let off = ((i << 8) as i32 >> 6) as u32 | ((i >> 23) & 2);
            self.regs[14] = self.pc.wrapping_add(4);
            let t = self.pc.wrapping_add(8).wrapping_add(off);
            self.thumb = true;
            self.jump(t);
            return Ok(());
        }
        if i & 0x0D70_F000 == 0x0550_F000 {
            return Ok(()); // PLD
        }
        Err(Ex::Undef)
    }

    pub(super) fn data_proc(&mut self, i: u32, op2: u32, sc: bool, rn_plus12: bool) -> R<()> {
        let op = (i >> 21) & 0xF;
        let s = i & (1 << 20) != 0;
        let rn = ((i >> 16) & 0xF) as usize;
        let rd = ((i >> 12) & 0xF) as usize;
        let a = if rn_plus12 { self.pc.wrapping_add(12) } else { self.r(rn) };
        let c = self.flag_c();
        let (res, write) = match op {
            0x0 | 0x8 => (a & op2, op == 0),
            0x1 | 0x9 => (a ^ op2, op == 1),
            0xC => (a | op2, true),
            0xD => (op2, true),
            0xE => (a & !op2, true),
            0xF => (!op2, true),
            _ => {
                let (r, cf, v) = match op {
                    0x2 | 0xA => add_with_carry(a, !op2, true),
                    0x3 => add_with_carry(op2, !a, true),
                    0x4 | 0xB => add_with_carry(a, op2, false),
                    0x5 => add_with_carry(a, op2, c),
                    0x6 => add_with_carry(a, !op2, c),
                    _ => add_with_carry(op2, !a, c), // RSC
                };
                let write = !matches!(op, 0xA | 0xB);
                if s && !(rd == 15 && write) {
                    self.set_nzcv(r, cf, v);
                }
                if write {
                    self.write_dp(rd, r, s);
                }
                return Ok(());
            }
        };
        if s && !(rd == 15 && write) {
            self.set_nzc(res, sc);
        }
        if write {
            self.write_dp(rd, res, s);
        }
        Ok(())
    }

    /// Result of a data-processing instruction; `movs pc, ...` returns from exceptions.
    #[inline(always)]
    fn write_dp(&mut self, rd: usize, v: u32, s: bool) {
        if rd == 15 && s {
            let spsr = self.spsr();
            self.set_cpsr(spsr);
            self.jump(v);
        } else {
            self.w(rd, v);
        }
    }

    pub(super) fn arm_misc(&mut self, i: u32) -> R<()> {
        let op = (i >> 4) & 0xF;
        let rd = ((i >> 12) & 0xF) as usize;
        let rm = (i & 0xF) as usize;
        match op {
            0 => {
                if i & 0x0020_0000 != 0 {
                    let v = self.r(rm);
                    return self.arm_msr(i, v);
                }
                // MRS
                let v = if i & (1 << 22) != 0 { self.spsr() } else { self.cpsr_full() };
                self.w(rd, v);
                Ok(())
            }
            1 => match (i >> 21) & 3 {
                1 => {
                    self.bx(self.r(rm)); // BX
                    Ok(())
                }
                3 => {
                    let z = self.r(rm).leading_zeros(); // CLZ
                    self.w(rd, z);
                    Ok(())
                }
                _ => Err(Ex::Undef),
            },
            2 if (i >> 21) & 3 == 1 => {
                self.bx(self.r(rm)); // BXJ: no Jazelle, plain BX
                Ok(())
            }
            3 if (i >> 21) & 3 == 1 => {
                let t = self.r(rm);
                self.regs[14] = self.pc.wrapping_add(4); // BLX reg
                self.bx(t);
                Ok(())
            }
            5 => {
                // QADD / QSUB / QDADD / QDSUB
                let rn = ((i >> 16) & 0xF) as usize;
                let (a, b) = (self.r(rm) as i32, self.r(rn) as i32);
                let (b, q0) = if i & (1 << 22) != 0 { sat_add(b, b) } else { (b as u32, false) };
                let (r, q) = if i & (1 << 21) != 0 { sat_sub(a, b as i32) } else { sat_add(a, b as i32) };
                if q || q0 {
                    self.set_q();
                }
                self.w(rd, r);
                Ok(())
            }
            7 if (i >> 21) & 3 == 1 => Err(Ex::Bkpt),
            8 | 0xA | 0xC | 0xE => self.arm_dsp_mul(i),
            _ => Err(Ex::Undef),
        }
    }

    /// SMLAxy / SMLAWy / SMULWy / SMLALxy / SMULxy.
    fn arm_dsp_mul(&mut self, i: u32) -> R<()> {
        let rd = ((i >> 16) & 0xF) as usize;
        let rn = ((i >> 12) & 0xF) as usize;
        let rs = ((i >> 8) & 0xF) as usize;
        let rm = (i & 0xF) as usize;
        let x = i & (1 << 5) != 0;
        let y = i & (1 << 6) != 0;
        let half = |v: u32, top: bool| -> i32 { if top { (v as i32) >> 16 } else { v as i16 as i32 } };
        let (m, s) = (self.r(rm), self.r(rs));
        match (i >> 21) & 3 {
            0 => {
                let p = half(m, x) * half(s, y);
                let (r, q) = sat_add_flag(p, self.r(rn) as i32);
                if q {
                    self.set_q();
                }
                self.w(rd, r);
            }
            1 => {
                let p = ((m as i32 as i64 * half(s, y) as i64) >> 16) as i32;
                if x {
                    self.w(rd, p as u32); // SMULWy
                } else {
                    let (r, q) = sat_add_flag(p, self.r(rn) as i32);
                    if q {
                        self.set_q();
                    }
                    self.w(rd, r);
                }
            }
            2 => {
                let p = half(m, x) as i64 * half(s, y) as i64;
                let acc = ((self.r(rd) as u64) << 32 | self.r(rn) as u64) as i64;
                let r = acc.wrapping_add(p) as u64;
                self.w(rn, r as u32);
                self.w(rd, (r >> 32) as u32);
            }
            _ => {
                let p = half(m, x) * half(s, y);
                self.w(rd, p as u32);
            }
        }
        Ok(())
    }

    pub(super) fn arm_msr(&mut self, i: u32, v: u32) -> R<()> {
        let fields = (i >> 16) & 0xF;
        let mut mask = 0u32;
        for b in 0..4 {
            if fields & (1 << b) != 0 {
                mask |= 0xFF << (8 * b);
            }
        }
        if i & (1 << 22) != 0 {
            let s = self.spsr();
            self.set_spsr((s & !mask) | (v & mask));
        } else {
            if self.is_user() {
                mask &= 0xF800_0000;
            }
            mask &= !0x20; // T is not writable by MSR
            let c = self.cpsr_full();
            self.set_cpsr((c & !mask) | (v & mask));
        }
        Ok(())
    }

    pub(super) fn arm_mul_or_extra(&mut self, i: u32) -> R<()> {
        let sh = (i >> 5) & 3;
        if sh == 0 {
            if i & 0x0F00_0000 == 0x0100_0000 {
                // SWP / SWPB
                let rn = ((i >> 16) & 0xF) as usize;
                let rd = ((i >> 12) & 0xF) as usize;
                let rm = (i & 0xF) as usize;
                let a = self.r(rn);
                let src = self.r(rm);
                if i & (1 << 22) != 0 {
                    let old = self.ld8(a)?;
                    self.st8(a, src)?;
                    self.w(rd, old);
                } else {
                    let old = self.ld32(a)?;
                    self.st32(a, src)?;
                    self.w(rd, old);
                }
                return Ok(());
            }
            let rd = ((i >> 16) & 0xF) as usize;
            let rn = ((i >> 12) & 0xF) as usize;
            let rs = ((i >> 8) & 0xF) as usize;
            let rm = (i & 0xF) as usize;
            let s = i & (1 << 20) != 0;
            if i & (1 << 23) == 0 {
                // MUL / MLA
                let mut r = self.r(rm).wrapping_mul(self.r(rs));
                if i & (1 << 21) != 0 {
                    r = r.wrapping_add(self.r(rn));
                }
                self.w(rd, r);
                if s {
                    self.set_nz(r);
                }
            } else {
                // UMULL / UMLAL / SMULL / SMLAL (rd = hi, rn = lo)
                let (a, b) = (self.r(rm), self.r(rs));
                let mut r = if i & (1 << 22) != 0 {
                    (a as i32 as i64).wrapping_mul(b as i32 as i64) as u64
                } else {
                    a as u64 * b as u64
                };
                if i & (1 << 21) != 0 {
                    r = r.wrapping_add((self.r(rd) as u64) << 32 | self.r(rn) as u64);
                }
                self.w(rn, r as u32);
                self.w(rd, (r >> 32) as u32);
                if s {
                    self.cpsr = (self.cpsr & 0x3FFF_FFFF) | ((r >> 32) as u32 & 0x8000_0000) | ((r == 0) as u32) << 30;
                }
            }
            return Ok(());
        }
        // halfword / signed / doubleword transfers
        let p = i & (1 << 24) != 0;
        let u = i & (1 << 23) != 0;
        let wb = i & (1 << 21) != 0;
        let l = i & (1 << 20) != 0;
        let rn = ((i >> 16) & 0xF) as usize;
        let rd = ((i >> 12) & 0xF) as usize;
        let off = if i & (1 << 22) != 0 { ((i >> 4) & 0xF0) | (i & 0xF) } else { self.r((i & 0xF) as usize) };
        let base = self.r(rn);
        let moved = if u { base.wrapping_add(off) } else { base.wrapping_sub(off) };
        let addr = if p { moved } else { base };
        let writeback = !p || wb;
        match (l, sh) {
            (true, _) => {
                let v = match sh {
                    1 => self.ld16(addr)?,
                    2 => self.ld8(addr)? as u8 as i8 as i32 as u32,
                    _ => self.ld16(addr)? as u16 as i16 as i32 as u32,
                };
                if writeback {
                    self.w(rn, moved);
                }
                self.w(rd, v);
            }
            (false, 1) => {
                let v = self.r(rd);
                self.st16(addr, v)?;
                if writeback {
                    self.w(rn, moved);
                }
            }
            (false, 2) => {
                // LDRD
                let lo = self.ld32(addr)?;
                let hi = self.ld32(addr.wrapping_add(4))?;
                if writeback {
                    self.w(rn, moved);
                }
                self.w(rd & !1, lo);
                self.w((rd & !1) + 1, hi);
            }
            _ => {
                // STRD
                let (lo, hi) = (self.r(rd & !1), self.r((rd & !1) + 1));
                self.st32(addr, lo)?;
                self.st32(addr.wrapping_add(4), hi)?;
                if writeback {
                    self.w(rn, moved);
                }
            }
        }
        Ok(())
    }

    pub(super) fn arm_ldr_str(&mut self, i: u32, off: u32) -> R<()> {
        let p = i & (1 << 24) != 0;
        let u = i & (1 << 23) != 0;
        let byte = i & (1 << 22) != 0;
        let wb = i & (1 << 21) != 0;
        let l = i & (1 << 20) != 0;
        let rn = ((i >> 16) & 0xF) as usize;
        let rd = ((i >> 12) & 0xF) as usize;
        let base = self.r(rn);
        let moved = if u { base.wrapping_add(off) } else { base.wrapping_sub(off) };
        let addr = if p { moved } else { base };
        let user = !p && wb; // LDRT / STRT
        let writeback = !p || wb;
        if l {
            let v = match (byte, user) {
                (true, false) => self.ld8(addr)?,
                (false, false) => self.ld32(addr)?,
                (true, true) => self.ld_user(addr, 1)?,
                (false, true) => self.ld_user(addr, 4)?,
            };
            if writeback {
                self.w(rn, moved);
            }
            if rd == 15 {
                self.bx(v);
            } else {
                self.regs[rd] = v;
            }
        } else {
            let v = self.r(rd);
            match (byte, user) {
                (true, false) => self.st8(addr, v)?,
                (false, false) => self.st32(addr, v)?,
                (true, true) => self.st_user(addr, 1, v)?,
                (false, true) => self.st_user(addr, 4, v)?,
            }
            if writeback {
                self.w(rn, moved);
            }
        }
        Ok(())
    }

    pub(super) fn arm_ldm_stm(&mut self, i: u32) -> R<()> {
        let p = i & (1 << 24) != 0;
        let u = i & (1 << 23) != 0;
        let s = i & (1 << 22) != 0;
        let wb = i & (1 << 21) != 0;
        let l = i & (1 << 20) != 0;
        let rn = ((i >> 16) & 0xF) as usize;
        let list = i & 0xFFFF;
        let n = list.count_ones();
        let base = self.regs[rn];
        let (start, new_base) = match (p, u) {
            (false, true) => (base, base.wrapping_add(4 * n)),
            (true, true) => (base.wrapping_add(4), base.wrapping_add(4 * n)),
            (false, false) => (base.wrapping_sub(4 * n).wrapping_add(4), base.wrapping_sub(4 * n)),
            (true, false) => (base.wrapping_sub(4 * n), base.wrapping_sub(4 * n)),
        };
        let mut a = start;
        if l {
            let user_bank = s && list & 0x8000 == 0;
            let mut vals = [0u32; 16];
            for r in 0..16 {
                if list & (1 << r) != 0 {
                    vals[r] = self.ld32(a)?;
                    a = a.wrapping_add(4);
                }
            }
            if wb && list & (1 << rn) == 0 {
                self.regs[rn] = new_base;
            }
            for r in 0..15 {
                if list & (1 << r) != 0 {
                    if user_bank {
                        self.set_user_reg(r, vals[r]);
                    } else {
                        self.regs[r] = vals[r];
                    }
                }
            }
            if list & 0x8000 != 0 {
                if s {
                    let spsr = self.spsr();
                    self.set_cpsr(spsr);
                    self.jump(vals[15]);
                } else {
                    self.bx(vals[15]);
                }
            }
        } else {
            for r in 0..16 {
                if list & (1 << r) != 0 {
                    let v = if r == 15 {
                        self.pc.wrapping_add(8)
                    } else if s {
                        self.user_reg(r)
                    } else {
                        self.regs[r]
                    };
                    self.st32(a, v)?;
                    a = a.wrapping_add(4);
                }
            }
            if wb {
                self.regs[rn] = new_base;
            }
        }
        Ok(())
    }

    pub(super) fn arm_mcr_mrc(&mut self, i: u32) -> R<()> {
        let cp = (i >> 8) & 0xF;
        if cp != 15 || self.is_user() {
            return Err(Ex::Undef);
        }
        let opc1 = (i >> 21) & 7;
        let crn = (i >> 16) & 0xF;
        let rd = ((i >> 12) & 0xF) as usize;
        let opc2 = (i >> 5) & 7;
        let crm = i & 0xF;
        if i & (1 << 20) != 0 {
            let v = self.cp15_read(opc1, crn, crm, opc2).ok_or(Ex::Undef)?;
            if rd == 15 {
                self.cpsr = (self.cpsr & 0x0FFF_FFFF) | (v & 0xF000_0000);
            } else {
                self.regs[rd] = v;
            }
            Ok(())
        } else {
            let v = self.r(rd);
            match self.cp15_write(opc1, crn, crm, opc2, v) {
                Ok(true) => Err(Ex::Wfi),
                Ok(false) => Ok(()),
                Err(()) => Err(Ex::Undef),
            }
        }
    }
}

#[inline(always)]
fn sat_add_flag(a: i32, b: i32) -> (u32, bool) {
    match a.checked_add(b) {
        Some(r) => (r as u32, false),
        None => (a.wrapping_add(b) as u32, true), // SMLA: wraps, sets Q
    }
}
