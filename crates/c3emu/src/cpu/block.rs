//! Execution engine: decoded-block cache and the run loop.
//!
//! A block is the straight-line code from an address up to the first instruction
//! that changes the flow (branch, write to pc, SWI, coprocessor access), the end of
//! the 4 KB page, or the next address with a code hook. It is decoded once into a list
//! of operations (handler + instruction word) and cached by virtual address, checked
//! against the current translation (physical address) and a per-page generation.
//!
//! Self-modifying code: every block marks the 256-byte physical lines it covers;
//! a store into a marked line bumps that page's generation (its blocks are stale) and
//! ends the current block after the storing instruction. So executing a block gives
//! exactly what executing its instructions one by one from memory would.

use std::cell::Cell;

use super::{uc_error, Cpu, Ex, R, PAGE_BITS};

/// An operation: the handler for one instruction and its encoding.
pub(super) type OpFn = for<'a, 'c> fn(&'c mut Cpu<'a>, u32) -> R<()>;

#[derive(Clone, Copy)]
pub(super) struct Op {
    f: OpFn,
    raw: u32,
}

pub(super) struct Block {
    va: u32,
    pa: u32,
    thumb: bool,
    generation: u32,
    epoch: u32,
    /// The first address has code hooks.
    hooked: bool,
    ops: Box<[Op]>,
    /// The block that followed last time: (its address, Cpu::world then, block).
    succ: Cell<(u32, u64, *const Block)>,
}

pub(super) const BLOCKS: usize = 1 << 16;
/// Decoded blocks kept before the whole cache is dropped and rebuilt.
const ARENA_MAX: usize = 1 << 18;
const MAX_OPS: usize = 64;
/// Code lines (256 bytes) for self-modifying-code detection.
pub(super) const LINE_BITS: u32 = 8;

#[inline(always)]
fn slot(va: u32) -> usize {
    ((va >> 1) ^ (va >> 15)) as usize & (BLOCKS - 1)
}

impl<'a> Cpu<'a> {
    /// A store hit physical address `pa`: if code was decoded from its line, the
    /// page's blocks are stale. Called on every RAM store (fast and slow paths).
    #[inline(always)]
    pub(super) fn smc_check(&mut self, pa: u32) {
        let line = (pa >> LINE_BITS) as usize;
        if self.code_lines[line >> 6] & (1 << (line & 63)) != 0 {
            self.smc(pa);
        }
    }

    #[cold]
    fn smc(&mut self, pa: u32) {
        let page = (pa >> PAGE_BITS) as usize;
        self.page_gen[page] = self.page_gen[page].wrapping_add(1);
        // the 16 lines of the page are 16 bits of one word
        let first = page << (PAGE_BITS - LINE_BITS);
        self.code_lines[first >> 6] &= !(0xFFFFu64 << (first & 63));
        self.brk = true; // leave the (possibly stale) current block
        self.world += 1;
        self.stat_smc += 1;
    }

    /// Forget every decoded block (hooks changed).
    pub(super) fn flush_blocks(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.world += 1;
    }

    #[inline(always)]
    fn valid(&self, b: &Block) -> bool {
        b.thumb == self.thumb && b.epoch == self.epoch && b.generation == self.page_gen[(b.pa >> PAGE_BITS) as usize]
    }

    /// The block at pc (decoding it if needed), `prev` the block that ran before (its
    /// successor link is tried first and updated). Err: prefetch abort / unmapped.
    /// Blocks live in the arena until it is dropped as a whole (here, and only when
    /// no block is running), so the pointers stay valid meanwhile.
    fn block(&mut self, prev: &mut *const Block) -> R<*const Block> {
        let va = self.pc;
        if !prev.is_null() {
            // nothing that could invalidate the link happened since (TLB flush, code
            // overwritten, hooks changed): the address alone identifies the block
            let (sva, world, sb) = unsafe { (**prev).succ.get() };
            if sva == va && world == self.world && unsafe { (*sb).thumb } == self.thumb {
                self.stat_chained += 1;
                return Ok(sb);
            }
        }
        let pa = self.fetch_pa(va)?;
        let s = slot(va);
        let mut b = self.blocks[s];
        if b.is_null() || unsafe { (*b).va != va || (*b).pa != pa } || !self.valid(unsafe { &*b }) {
            self.stat_decodes += 1;
            if self.arena.len() >= ARENA_MAX {
                self.blocks.fill(std::ptr::null());
                self.arena.clear();
                *prev = std::ptr::null();
            }
            let nb = Box::new(self.decode_block(va, pa));
            b = &*nb as *const Block;
            self.arena.push(nb);
            self.blocks[s] = b;
        }
        if !prev.is_null() {
            unsafe { (**prev).succ.set((va, self.world, b)) };
        }
        Ok(b)
    }

    fn decode_block(&mut self, va: u32, pa: u32) -> Block {
        let thumb = self.thumb;
        let w = if thumb { 2 } else { 4 };
        let page = pa & !0xFFF;
        let ptr = self.page_ptr(page);
        let mut ops = Vec::with_capacity(16);
        let mut off = pa & 0xFFF;
        let first_va = va;
        while (off as usize) + w as usize <= 4096 && ops.len() < MAX_OPS {
            let a = first_va.wrapping_add(off - (pa & 0xFFF));
            if !ops.is_empty() && self.has_addr_hook(a) {
                break; // hooked addresses start their own block
            }
            let raw = unsafe {
                if thumb {
                    u16::from_le((ptr.add(off as usize) as *const u16).read_unaligned()) as u32
                } else {
                    u32::from_le((ptr.add(off as usize) as *const u32).read_unaligned())
                }
            };
            let (f, ends): (OpFn, bool) = if thumb {
                (super::thumb::decode(raw as u16), super::thumb::ends_block(raw as u16))
            } else {
                (super::arm::decode(raw), super::arm::ends_block(raw))
            };
            ops.push(Op { f, raw });
            off += w;
            if ends {
                break;
            }
        }
        // from now on stores to this page take the slow path (with the SMC check)
        let p = (pa >> PAGE_BITS) as usize;
        if self.code_page[p] == 0 {
            self.code_page[p] = 1;
            for (e, f) in self.tlb.iter_mut().zip(self.tlbf.iter_mut()) {
                if e.pa == page && e.vpn != u32::MAX {
                    e.wtag = u32::MAX;
                    f.wtag = u32::MAX;
                }
            }
        }
        // mark the code lines
        let (lo, hi) = (pa >> LINE_BITS, (page + off - 1) >> LINE_BITS);
        for line in lo..=hi {
            let l = line as usize;
            self.code_lines[l >> 6] |= 1 << (l & 63);
        }
        Block {
            va,
            pa,
            thumb,
            generation: self.page_gen[(pa >> PAGE_BITS) as usize],
            epoch: self.epoch,
            hooked: self.has_addr_hook(va),
            ops: ops.into_boxed_slice(),
            succ: Cell::new((0, u64::MAX, std::ptr::null())),
        }
    }

    /// Run from `begin` (bit 0 = Thumb) until `until`, `timeout` microseconds (0 = no
    /// limit), `count` instructions (0 = no limit), WFI, or emu_stop.
    pub fn emu_start(&mut self, begin: u64, until: u64, timeout: u64, count: usize) -> Result<(), uc_error> {
        self.stop = false;
        self.err = None;
        self.wfi = false;
        self.thumb = begin & 1 != 0;
        self.pc = begin as u32 & !1;
        let deadline = (timeout > 0).then(|| crate::clock::Instant::now() + std::time::Duration::from_micros(timeout));
        let limit = if count > 0 { self.insn_limit.min(self.icount + count as u64) } else { self.insn_limit };
        let mut next_time_check = self.icount + 0x4000;
        let mut first = true;
        let until = until as u32 & !1;
        let trace_all = !self.all_code_hooks.is_empty();
        let mut prev: *const Block = std::ptr::null();
        'run: loop {
            if self.stop || self.pc == until || self.icount >= limit {
                break;
            }
            if let Some(d) = deadline {
                if self.icount >= next_time_check {
                    next_time_check = self.icount + 0x4000;
                    if crate::clock::Instant::now() >= d {
                        break;
                    }
                }
            }
            let blk = match self.block(&mut prev) {
                // SAFETY: the arena is only dropped by self.block(), not while a block runs
                Ok(b) => {
                    prev = b;
                    unsafe { &*b }
                }
                Err(e) => {
                    if self.exception(e) {
                        break;
                    }
                    first = false;
                    continue;
                }
            };
            self.stat_blocks += 1;
            let pc0 = self.pc;
            let w = if blk.thumb { 2 } else { 4 };
            if !self.block_hooks.is_empty() {
                let hs = self.block_hooks.clone();
                self.jumped = false;
                let size = blk.ops.len() as u32 * w;
                for h in hs {
                    (h.borrow_mut())(self, pc0 as u64, size);
                }
                if self.stop {
                    break;
                }
                if self.pc != pc0 || self.jumped {
                    self.jumped = false;
                    first = false;
                    continue;
                }
            }
            if !first && (blk.hooked || trace_all) && self.code_hooks(pc0, w) {
                if self.stop {
                    break;
                }
                self.jumped = false;
                continue;
            }
            first = false;
            self.jumped = false;
            self.brk = false;
            let mut pc = pc0;
            let room = (limit - self.icount).min(blk.ops.len() as u64) as usize;
            for (n, op) in blk.ops[..room].iter().enumerate() {
                if n > 0 && trace_all {
                    if self.code_hooks(pc, w) {
                        if self.stop {
                            break 'run;
                        }
                        self.jumped = false;
                        continue 'run;
                    }
                }
                self.pc = pc;
                let r = (op.f)(self, op.raw);
                self.icount += 1;
                if let Err(e) = r {
                    self.jumped = false;
                    self.brk = false;
                    if self.exception(e) {
                        break 'run;
                    }
                    continue 'run;
                }
                if self.jumped | self.brk {
                    self.pc = if self.jumped { self.next } else { pc.wrapping_add(w) };
                    self.jumped = false;
                    self.brk = false;
                    continue 'run;
                }
                pc = pc.wrapping_add(w);
            }
            self.pc = pc;
        }
        match self.err.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// An instruction did not complete normally (pc = that instruction). Returns true
    /// if the run must end.
    fn exception(&mut self, e: Ex) -> bool {
        match e {
            Ex::Wfi => {
                self.pc = self.pc.wrapping_add(4); // mcr: ARM
                self.wfi = true;
                true
            }
            Ex::Swi => {
                self.pc = self.pc.wrapping_add(if self.thumb { 2 } else { 4 });
                self.intr(2);
                false
            }
            Ex::Bkpt => {
                self.intr(7);
                false
            }
            Ex::DataAbort => {
                self.intr(4);
                false
            }
            Ex::PrefetchAbort => {
                self.intr(3);
                false
            }
            Ex::Undef => {
                let hs = self.invalid_hooks.clone();
                for h in hs {
                    if (h.borrow_mut())(self) {
                        return false;
                    }
                }
                self.err = Some(uc_error::INSN_INVALID);
                true
            }
            Ex::Err(e) => {
                self.err = Some(e);
                true
            }
        }
    }
}
