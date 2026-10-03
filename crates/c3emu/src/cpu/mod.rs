//! ARM926EJ-S (ARMv5TE) interpreter offering the subset of the Unicorn API c3emu uses,
//! so the same machine runs where Unicorn cannot (WebAssembly has no JIT). Selected by
//! building without the `unicorn` feature (see uc.rs).
//!
//! Where c3emu depends on Unicorn/QEMU behaviour, the same is done here:
//! * the code hook of the instruction an emu_start begins at does not run (see
//!   Machine::prehook); a code hook that moves pc or calls emu_stop prevents its
//!   instruction from running;
//! * UNDEF (incl. coprocessor registers the ARM926 model lacks) goes to the invalid-
//!   instruction hooks, else emu_start fails with INSN_INVALID. SWI / BKPT / aborts go
//!   to the interrupt hooks *without* the architectural exception entry (pc = next
//!   instruction for SWI, the faulting one otherwise); execution continues at whatever
//!   pc the hook left;
//! * WFI (mcr p15,0,rd,c7,c0,4) ends emu_start with pc at the next instruction;
//! * mem_read / mem_write take physical addresses; a write to pc selects Thumb by bit 0;
//! * unaligned loads are not rotated (QEMU's ARMv5 model does not rotate either).

use std::cell::RefCell;
use crate::fxhash::FxHashMap as HashMap;
use std::marker::PhantomData;
use std::rc::Rc;

mod arm;
mod block;
mod thumb;

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum uc_error {
    OK,
    NOMEM,
    ARG,
    READ_UNMAPPED,
    WRITE_UNMAPPED,
    FETCH_UNMAPPED,
    INSN_INVALID,
    EXCEPTION,
}

/// Register ids (same numbers as Unicorn's ARM ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(pub i32);

impl From<Reg> for i32 {
    fn from(r: Reg) -> i32 {
        r.0
    }
}

pub struct RegisterARM;

impl RegisterARM {
    pub const CPSR: Reg = Reg(3);
    pub const LR: Reg = Reg(10);
    pub const PC: Reg = Reg(11);
    pub const SP: Reg = Reg(12);
    pub const SPSR: Reg = Reg(13);
    pub const R0: Reg = Reg(66);
    pub const R1: Reg = Reg(67);
    pub const R2: Reg = Reg(68);
    pub const R3: Reg = Reg(69);
    pub const R4: Reg = Reg(70);
    pub const R5: Reg = Reg(71);
    pub const R6: Reg = Reg(72);
    pub const R7: Reg = Reg(73);
    pub const R8: Reg = Reg(74);
    pub const R9: Reg = Reg(75);
    pub const R10: Reg = Reg(76);
    pub const R11: Reg = Reg(77);
    pub const R12: Reg = Reg(78);
    pub const R13: Reg = Reg(12);
    pub const R14: Reg = Reg(10);
    pub const R15: Reg = Reg(11);
}

/// Coprocessor register (as Unicorn's uc_arm_cp_reg).
#[derive(Debug, Clone, Copy, Default)]
pub struct RegisterARMCP {
    pub cp: u32,
    pub is64: u32,
    pub sec: u32,
    pub crn: u32,
    pub crm: u32,
    pub opc1: u32,
    pub opc2: u32,
    pub val: u64,
}

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemType {
    READ,
    WRITE,
    FETCH,
    READ_UNMAPPED,
    WRITE_UNMAPPED,
    FETCH_UNMAPPED,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookType(pub i32);

impl HookType {
    pub const MEM_READ_UNMAPPED: HookType = HookType(16);
    pub const MEM_WRITE_UNMAPPED: HookType = HookType(32);
    pub const MEM_FETCH_UNMAPPED: HookType = HookType(64);
    pub const MEM_UNMAPPED: HookType = HookType(16 | 32 | 64);
    pub const MEM_READ: HookType = HookType(1 << 10);
    pub const MEM_WRITE: HookType = HookType(1 << 11);
}

impl std::ops::BitOr for HookType {
    type Output = HookType;
    fn bitor(self, o: HookType) -> HookType {
        HookType(self.0 | o.0)
    }
}

pub struct Prot;

impl Prot {
    pub const ALL: u32 = 7;
}

pub type UcHookId = usize;

type CodeCb<'a> = Rc<RefCell<dyn FnMut(&mut Cpu<'a>, u64, u32) + 'a>>;
type IntrCb<'a> = Rc<RefCell<dyn FnMut(&mut Cpu<'a>, u32) + 'a>>;
type InvalidCb<'a> = Rc<RefCell<dyn FnMut(&mut Cpu<'a>) -> bool + 'a>>;
type MemCb<'a> = Rc<RefCell<dyn FnMut(&mut Cpu<'a>, MemType, u64, usize, i64) -> bool + 'a>>;
type MmioRead<'a> = Rc<RefCell<dyn FnMut(&mut Cpu<'a>, u64, usize) -> u64 + 'a>>;
type MmioWrite<'a> = Rc<RefCell<dyn FnMut(&mut Cpu<'a>, u64, usize, u64) + 'a>>;

enum Hook<'a> {
    /// Code hook on [begin, end] (begin > end: every instruction).
    Code(u64, u64, CodeCb<'a>),
    Block(CodeCb<'a>),
    Intr(IntrCb<'a>),
    Invalid(InvalidCb<'a>),
    /// Memory hook: (HookType bits, begin, end) — on virtual addresses.
    Mem(i32, u64, u64, MemCb<'a>),
}

struct Mmio<'a> {
    base: u64,
    read: Option<MmioRead<'a>>,
    write: Option<MmioWrite<'a>>,
}

/// Why an instruction did not complete normally.
pub(crate) enum Ex {
    Undef,
    Swi,
    Bkpt,
    DataAbort,
    PrefetchAbort,
    Wfi,
    Err(uc_error),
}

pub(crate) type R<T> = Result<T, Ex>;

const PAGE_BITS: u32 = 12;
const PAGES: usize = 1 << (32 - PAGE_BITS);
const TLB_N: usize = 4096;
/// Physical page kinds.
const UNMAPPED: u8 = 0;
const RAM: u8 = 1;
/// TLB permission bits.
const P_PR: u8 = 1;
const P_PW: u8 = 2;
const P_UR: u8 = 4;
const P_UW: u8 = 8;

#[derive(Clone, Copy)]
struct Tlb {
    vpn: u32,
    /// = vpn when loads / stores in the current privilege may use `host` directly
    /// (RAM, permitted, no memory hooks, no alignment checking; stores also: no code
    /// decoded from the page), else NO_TAG.
    wtag: u32,
    /// Host pointer to the physical page (RAM only; null = slow path).
    host: *mut u8,
    /// Physical page base.
    pa: u32,
    perm: u8,
}

/// The fast-path part of a TLB entry (16 bytes, kept apart for the cache).
#[derive(Clone, Copy)]
struct TlbFast {
    rtag: u32,
    wtag: u32,
    host: *mut u8,
}

const NO_TAG: u32 = u32::MAX;
const TLBF_INVALID: TlbFast = TlbFast { rtag: u32::MAX, wtag: u32::MAX, host: std::ptr::null_mut() };
const TLB_INVALID: Tlb = Tlb { vpn: NO_TAG, wtag: NO_TAG, host: std::ptr::null_mut(), pa: 0, perm: 0 };

/// Condition-code table: bit `nzcv` of COND[cond] is set when `cond` passes.
const COND: [u16; 16] = {
    let mut t = [0u16; 16];
    let mut f = 0;
    while f < 16 {
        let (n, z, c, v) = (f & 8 != 0, f & 4 != 0, f & 2 != 0, f & 1 != 0);
        let pass = [z, !z, c, !c, n, !n, v, !v, c && !z, !c || z, n == v, n != v, !z && n == v, z || n != v, true, true];
        let mut cc = 0;
        while cc < 16 {
            if pass[cc] {
                t[cc] |= 1 << f;
            }
            cc += 1;
        }
        f += 1;
    }
    t
};

pub struct Cpu<'a> {
    /// r0-r14 of the current mode (index 15 unused: see `pc`).
    regs: [u32; 16],
    /// Address of the instruction being executed (or the next one, between runs).
    pc: u32,
    /// Where execution continues after the current instruction.
    next: u32,
    /// The current instruction changed the flow (a new block starts).
    jumped: bool,
    /// Leave the current block after this instruction (stop request, interrupt, code
    /// overwritten), continuing at the next instruction.
    brk: bool,
    thumb: bool,
    /// CPSR without the T bit.
    cpsr: u32,
    spsr: [u32; 6],
    bank13: [u32; 6],
    bank14: [u32; 6],
    usr8: [u32; 5],
    fiq8: [u32; 5],
    // CP15
    sctlr: u32,
    ttbr: u32,
    dacr: u32,
    dfsr: u32,
    ifsr: u32,
    far: u32,
    fcse: u32,
    ctxid: u32,
    c9: [u32; 2],
    c10: u32,
    // memory
    kind: Vec<u8>,
    pages: Vec<Option<Box<[u8; 4096]>>>,
    mmio: Vec<Mmio<'a>>,
    tlb: Vec<Tlb>,
    tlbf: Vec<TlbFast>,
    /// Permission bit checked for loads / stores in the current mode.
    rd_perm: u8,
    wr_perm: u8,
    // hooks
    hooks: Vec<Option<Hook<'a>>>,
    addr_hooks: HashMap<u32, Rc<[CodeCb<'a>]>>,
    /// Hooked instruction addresses: per page an index into `hook_bits` (0 = none).
    hook_l1: Vec<u32>,
    hook_bits: Vec<[u64; 32]>,
    all_code_hooks: Vec<(u64, u64, CodeCb<'a>)>,
    block_hooks: Vec<CodeCb<'a>>,
    intr_hooks: Vec<IntrCb<'a>>,
    invalid_hooks: Vec<InvalidCb<'a>>,
    unmapped_hooks: Vec<MemCb<'a>>,
    rw_hooks: Vec<(i32, u64, u64, MemCb<'a>)>,
    // decoded blocks (block.rs)
    blocks: Vec<*const block::Block>,
    arena: Vec<Box<block::Block>>,
    /// Bumped on every TLB flush, code overwrite or hook change: validates the
    /// blocks' successor links.
    world: u64,
    pub stat_smc: u64,
    pub stat_chained: u64,
    /// Bit per 256-byte physical line some block was decoded from.
    code_lines: Vec<u64>,
    /// Per physical page: bumped when code in it is overwritten.
    page_gen: Vec<u32>,
    /// Bumped to drop every block (hooks changed).
    epoch: u32,
    /// Per physical page: some block was decoded from it (its stores take the slow path).
    code_page: Vec<u8>,
    // run control
    /// Instructions retired (exact; the exact mode's clock).
    pub icount: u64,
    /// Statistics: blocks entered, blocks decoded.
    pub stat_blocks: u64,
    pub stat_decodes: u64,
    pub stat_slow: u64,
    /// The last emu_start ended on WFI.
    pub wfi: bool,
    /// emu_start returns once icount reaches this.
    pub insn_limit: u64,
    /// An interrupt is pending: stop as soon as the CPU unmasks IRQs (exact mode).
    irq_line: bool,
    // run control
    stop: bool,
    err: Option<uc_error>,
    _life: PhantomData<&'a ()>,
}

fn mode_index(mode: u32) -> usize {
    match mode & 0x1F {
        0x11 => 1,
        0x12 => 2,
        0x13 => 3,
        0x17 => 4,
        0x1B => 5,
        _ => 0,
    }
}

impl<'a> Default for Cpu<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Cpu<'a> {
    pub fn new() -> Self {
        Cpu {
            regs: [0; 16],
            pc: 0,
            next: 0,
            jumped: false,
            brk: false,
            thumb: false,
            cpsr: 0x1D3, // svc, I+F masked (QEMU reset state)
            spsr: [0; 6],
            bank13: [0; 6],
            bank14: [0; 6],
            usr8: [0; 5],
            fiq8: [0; 5],
            sctlr: 0x0009_0078,
            ttbr: 0,
            dacr: 0,
            dfsr: 0,
            ifsr: 0,
            far: 0,
            fcse: 0,
            ctxid: 0,
            c9: [0; 2],
            c10: 0,
            kind: vec![UNMAPPED; PAGES],
            pages: (0..PAGES).map(|_| None).collect(),
            mmio: Vec::new(),
            tlb: vec![TLB_INVALID; TLB_N],
            tlbf: vec![TLBF_INVALID; TLB_N],
            rd_perm: P_PR,
            wr_perm: P_PW,
            hooks: Vec::new(),
            addr_hooks: HashMap::default(),
            hook_l1: vec![0; PAGES],
            hook_bits: vec![[0; 32]],
            all_code_hooks: Vec::new(),
            block_hooks: Vec::new(),
            intr_hooks: Vec::new(),
            invalid_hooks: Vec::new(),
            unmapped_hooks: Vec::new(),
            rw_hooks: Vec::new(),
            blocks: vec![std::ptr::null(); block::BLOCKS],
            arena: Vec::new(),
            world: 0,
            stat_smc: 0,
            stat_chained: 0,
            code_lines: vec![0; 1 << (32 - block::LINE_BITS - 6)],
            page_gen: vec![0; PAGES],
            epoch: 0,
            code_page: vec![0; PAGES],
            icount: 0,
            stat_blocks: 0,
            stat_decodes: 0,
            stat_slow: 0,
            insn_limit: u64::MAX,
            wfi: false,
            irq_line: false,
            stop: false,
            err: None,
            _life: PhantomData,
        }
    }

    // ----- registers -------------------------------------------------------------

    pub(crate) fn cpsr_full(&self) -> u32 {
        self.cpsr | (self.thumb as u32) << 5
    }

    /// Write CPSR (mode switch with register banking; T bit included).
    pub(crate) fn set_cpsr(&mut self, v: u32) {
        let (old, new) = (self.cpsr & 0x1F, v & 0x1F);
        if old != new {
            self.switch_bank(old, new);
        }
        self.cpsr = v & !0x20;
        self.thumb = v & 0x20 != 0;
        let user = new == 0x10;
        if (old == 0x10) != user {
            self.rd_perm = if user { P_UR } else { P_PR };
            self.wr_perm = if user { P_UW } else { P_PW };
            self.flush_tlb(); // the fast tags are per privilege
        }
        if self.irq_line && v & 0x80 == 0 {
            self.stop = true; // IRQs unmasked with one pending: take it now
            self.brk = true;
        }
    }

    fn switch_bank(&mut self, old: u32, new: u32) {
        let (oi, ni) = (mode_index(old), mode_index(new));
        self.bank13[oi] = self.regs[13];
        self.bank14[oi] = self.regs[14];
        if oi == 1 && ni != 1 {
            self.fiq8.copy_from_slice(&self.regs[8..13]);
            self.regs[8..13].copy_from_slice(&self.usr8);
        } else if oi != 1 && ni == 1 {
            self.usr8.copy_from_slice(&self.regs[8..13]);
            self.regs[8..13].copy_from_slice(&self.fiq8);
        }
        self.regs[13] = self.bank13[ni];
        self.regs[14] = self.bank14[ni];
    }

    pub(crate) fn spsr(&self) -> u32 {
        self.spsr[mode_index(self.cpsr)]
    }

    pub(crate) fn set_spsr(&mut self, v: u32) {
        let i = mode_index(self.cpsr);
        if i != 0 {
            self.spsr[i] = v;
        }
    }

    /// User-mode view of register n (LDM/STM with ^).
    pub(crate) fn user_reg(&self, n: usize) -> u32 {
        let mi = mode_index(self.cpsr);
        match n {
            8..=12 if mi == 1 => self.usr8[n - 8],
            13 if mi != 0 => self.bank13[0],
            14 if mi != 0 => self.bank14[0],
            _ => self.regs[n],
        }
    }

    pub(crate) fn set_user_reg(&mut self, n: usize, v: u32) {
        let mi = mode_index(self.cpsr);
        match n {
            8..=12 if mi == 1 => self.usr8[n - 8] = v,
            13 if mi != 0 => self.bank13[0] = v,
            14 if mi != 0 => self.bank14[0] = v,
            _ => self.regs[n] = v,
        }
    }

    /// Register as an operand: r15 reads as the current instruction + 8 (ARM) / + 4.
    #[inline(always)]
    pub(crate) fn r(&self, n: usize) -> u32 {
        if n == 15 { self.pc.wrapping_add(if self.thumb { 4 } else { 8 }) } else { self.regs[n] }
    }

    /// Write a register; r15 is a branch (no interworking).
    #[inline(always)]
    pub(crate) fn w(&mut self, n: usize, v: u32) {
        if n == 15 {
            self.jump(v);
        } else {
            self.regs[n] = v;
        }
    }

    #[inline(always)]
    pub(crate) fn jump(&mut self, v: u32) {
        self.next = v & if self.thumb { !1 } else { !3 };
        self.jumped = true;
    }

    /// Branch with interworking (BX, and loads into pc on ARMv5).
    #[inline(always)]
    pub(crate) fn bx(&mut self, v: u32) {
        self.thumb = v & 1 != 0;
        self.jump(v);
    }

    #[inline(always)]
    pub(crate) fn cond(&self, c: u32) -> bool {
        COND[c as usize] & (1 << (self.cpsr >> 28)) != 0
    }

    #[inline(always)]
    pub(crate) fn flag_c(&self) -> bool {
        self.cpsr & (1 << 29) != 0
    }

    #[inline(always)]
    pub(crate) fn set_nz(&mut self, v: u32) {
        self.cpsr = (self.cpsr & 0x3FFF_FFFF) | (v & 0x8000_0000) | ((v == 0) as u32) << 30;
    }

    #[inline(always)]
    pub(crate) fn set_nzc(&mut self, v: u32, c: bool) {
        self.cpsr = (self.cpsr & 0x1FFF_FFFF) | (v & 0x8000_0000) | ((v == 0) as u32) << 30 | (c as u32) << 29;
    }

    #[inline(always)]
    pub(crate) fn set_nzcv(&mut self, v: u32, c: bool, ov: bool) {
        self.cpsr = (self.cpsr & 0x0FFF_FFFF) | (v & 0x8000_0000) | ((v == 0) as u32) << 30
            | (c as u32) << 29 | (ov as u32) << 28;
    }

    pub(crate) fn set_q(&mut self) {
        self.cpsr |= 1 << 27;
    }

    pub(crate) fn is_user(&self) -> bool {
        self.cpsr & 0x1F == 0x10
    }

    fn reg_index(id: i32) -> Option<usize> {
        match id {
            66..=78 => Some((id - 66) as usize),
            12 => Some(13),
            10 => Some(14),
            _ => None,
        }
    }

    pub fn reg_read<T: Into<i32>>(&self, regid: T) -> Result<u64, uc_error> {
        let id = regid.into();
        Ok(match id {
            11 => self.pc as u64,
            3 => self.cpsr_full() as u64,
            13 => self.spsr() as u64,
            _ => self.regs[Self::reg_index(id).ok_or(uc_error::ARG)?] as u64,
        })
    }

    pub fn reg_write<T: Into<i32>>(&mut self, regid: T, value: u64) -> Result<(), uc_error> {
        let id = regid.into();
        let v = value as u32;
        match id {
            11 => {
                self.thumb = v & 1 != 0;
                self.pc = v & !1;
                self.next = self.pc;
                self.jumped = true;
            }
            3 => self.set_cpsr(v),
            13 => self.set_spsr(v),
            _ => self.regs[Self::reg_index(id).ok_or(uc_error::ARG)?] = v,
        }
        Ok(())
    }

    pub fn set_pc(&mut self, value: u64) -> Result<(), uc_error> {
        self.reg_write(RegisterARM::PC, value)
    }

    // ----- CP15 ------------------------------------------------------------------

    /// MRC p15. None: the ARM926 model has no such register (UNDEF).
    pub(crate) fn cp15_read(&self, opc1: u32, crn: u32, crm: u32, opc2: u32) -> Option<u32> {
        if crn == 15 {
            return Some(0); // implementation-defined test/debug registers
        }
        if opc1 != 0 {
            return None;
        }
        Some(match (crn, crm, opc2) {
            (0, 0, 0) | (0, 0, 3..=7) => 0x4106_9265,
            (0, 0, 1) => 0x01dd_20d2,
            (0, 0, 2) => 0,
            (1, 0, 0) => self.sctlr,
            (2, 0, 0) => self.ttbr,
            (3, 0, 0) => self.dacr,
            (5, 0, 0) => self.dfsr,
            (5, 0, 1) => self.ifsr,
            (6, 0, 0) => self.far,
            (7, 10 | 14, 3) => 1 << 30, // test (and clean): cache is clean
            (9, 0, n @ 0..=1) => self.c9[n as usize],
            (10, 0, 0) => self.c10,
            (13, 0, 0) => self.fcse,
            (13, 0, 1) => self.ctxid,
            _ => return None,
        })
    }

    /// MCR p15. Err(()) = UNDEF; Ok(true) = WFI.
    pub(crate) fn cp15_write(&mut self, opc1: u32, crn: u32, crm: u32, opc2: u32, v: u32) -> Result<bool, ()> {
        if crn == 15 {
            return Ok(false);
        }
        if opc1 != 0 {
            return Err(());
        }
        match (crn, crm, opc2) {
            (1, 0, 0) => {
                self.sctlr = v;
                self.flush_tlb();
            }
            (2, 0, 0) => {
                self.ttbr = v;
                self.flush_tlb();
            }
            (3, 0, 0) => {
                self.dacr = v;
                self.flush_tlb();
            }
            (5, 0, 0) => self.dfsr = v,
            (5, 0, 1) => self.ifsr = v,
            (6, 0, 0) => self.far = v,
            (7, 0, 4) => return Ok(true),
            (7, _, _) => {} // cache maintenance
            (8, _, _) => self.flush_tlb(),
            (9, 0, n @ 0..=1) => self.c9[n as usize] = v,
            (10, 0, 0) => self.c10 = v,
            (13, 0, 0) => {
                self.fcse = v & 0xFE00_0000;
                self.flush_tlb();
            }
            (13, 0, 1) => self.ctxid = v,
            _ => return Err(()),
        }
        Ok(false)
    }

    pub fn reg_read_arm_coproc(&self, r: &mut RegisterARMCP) -> Result<(), uc_error> {
        if r.cp != 15 {
            return Err(uc_error::ARG);
        }
        r.val = self.cp15_read(r.opc1, r.crn, r.crm, r.opc2).ok_or(uc_error::ARG)? as u64;
        Ok(())
    }

    pub fn reg_write_arm_coproc(&mut self, r: &RegisterARMCP) -> Result<(), uc_error> {
        if r.cp != 15 {
            return Err(uc_error::ARG);
        }
        self.cp15_write(r.opc1, r.crn, r.crm, r.opc2, r.val as u32).map(|_| ()).map_err(|_| uc_error::ARG)
    }

    // ----- memory: physical ------------------------------------------------------

    pub fn mem_map(&mut self, base: u64, size: u64, _prot: u32) -> Result<(), uc_error> {
        if base & 0xFFF != 0 || size & 0xFFF != 0 || base + size > 1 << 32 {
            return Err(uc_error::ARG);
        }
        for p in (base >> PAGE_BITS)..((base + size) >> PAGE_BITS) {
            self.kind[p as usize] = RAM;
        }
        self.flush_tlb();
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    pub fn mmio_map<RF, WF>(&mut self, base: u64, size: u64, read: Option<RF>, write: Option<WF>) -> Result<(), uc_error>
    where
        RF: FnMut(&mut Cpu<'a>, u64, usize) -> u64 + 'a,
        WF: FnMut(&mut Cpu<'a>, u64, usize, u64) + 'a,
    {
        if base & 0xFFF != 0 || size & 0xFFF != 0 || self.mmio.len() >= 250 {
            return Err(uc_error::ARG);
        }
        let idx = self.mmio.len() as u8 + 2;
        self.mmio.push(Mmio {
            base,
            read: read.map(|f| Rc::new(RefCell::new(f)) as MmioRead<'a>),
            write: write.map(|f| Rc::new(RefCell::new(f)) as MmioWrite<'a>),
        });
        for p in (base >> PAGE_BITS)..((base + size) >> PAGE_BITS) {
            self.kind[p as usize] = idx;
        }
        self.flush_tlb();
        Ok(())
    }

    fn page_ptr(&mut self, pa: u32) -> *mut u8 {
        let p = (pa >> PAGE_BITS) as usize;
        self.pages[p].get_or_insert_with(|| Box::new([0; 4096])).as_mut_ptr()
    }

    pub fn mem_read(&self, address: u64, buf: &mut [u8]) -> Result<(), uc_error> {
        let mut done = 0usize;
        while done < buf.len() {
            let a = address + done as u64;
            if a >= 1 << 32 {
                return Err(uc_error::READ_UNMAPPED);
            }
            let p = (a >> PAGE_BITS) as usize;
            let off = (a & 0xFFF) as usize;
            let n = (4096 - off).min(buf.len() - done);
            match self.kind[p] {
                UNMAPPED => return Err(uc_error::READ_UNMAPPED),
                RAM => match &self.pages[p] {
                    Some(pg) => buf[done..done + n].copy_from_slice(&pg[off..off + n]),
                    None => buf[done..done + n].fill(0),
                },
                _ => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }

    pub fn mem_read_as_vec(&self, address: u64, size: usize) -> Result<Vec<u8>, uc_error> {
        let mut v = vec![0; size];
        self.mem_read(address, &mut v)?;
        Ok(v)
    }

    pub fn mem_write(&mut self, address: u64, bytes: &[u8]) -> Result<(), uc_error> {
        let mut done = 0usize;
        while done < bytes.len() {
            let a = address + done as u64;
            if a >= 1 << 32 {
                return Err(uc_error::WRITE_UNMAPPED);
            }
            let p = (a >> PAGE_BITS) as usize;
            let off = (a & 0xFFF) as usize;
            let n = (4096 - off).min(bytes.len() - done);
            match self.kind[p] {
                RAM => {
                    let ptr = self.page_ptr(a as u32);
                    unsafe { std::ptr::copy_nonoverlapping(bytes[done..].as_ptr(), ptr.add(off), n) };
                    let mut l = a as u32 & !0xFF;
                    while (l as u64) < a + n as u64 {
                        self.smc_check(l);
                        l += 256;
                    }
                }
                UNMAPPED => return Err(uc_error::WRITE_UNMAPPED),
                _ => {}
            }
            done += n;
        }
        Ok(())
    }

    fn phys_word(&self, pa: u32) -> u32 {
        let mut b = [0u8; 4];
        let _ = self.mem_read(pa as u64, &mut b);
        u32::from_le_bytes(b)
    }

    // ----- MMU -------------------------------------------------------------------

    pub(crate) fn flush_tlb(&mut self) {
        self.tlb.fill(TLB_INVALID);
        self.tlbf.fill(TLBF_INVALID);
        self.world += 1;
    }

    /// Translate `va`. Ok((pa, perms, cacheable as a 4 KB TLB entry)) or Err(FSR).
    fn walk(&self, va: u32) -> Result<(u32, u8, bool, bool), u32> {
        if self.sctlr & 1 == 0 {
            return Ok((va, P_PR | P_PW | P_UR | P_UW, false, true));
        }
        let mva = if va < 0x0200_0000 { va | self.fcse } else { va };
        let l1 = self.phys_word((self.ttbr & 0xFFFF_C000) | ((mva >> 20) << 2));
        let domain = (l1 >> 5) & 0xF;
        let (pa, ap, page, tlbable) = match l1 & 3 {
            0 => return Err(0x5),
            2 => ((l1 & 0xFFF0_0000) | (mva & 0x000F_FFFF), (l1 >> 10) & 3, false, true),
            t => {
                let l2a = if t == 1 {
                    (l1 & 0xFFFF_FC00) | (((mva >> 12) & 0xFF) << 2)
                } else {
                    (l1 & 0xFFFF_F000) | (((mva >> 10) & 0x3FF) << 2)
                };
                let l2 = self.phys_word(l2a);
                match l2 & 3 {
                    0 => return Err(0x7 | domain << 4),
                    1 => ((l2 & 0xFFFF_0000) | (mva & 0xFFFF), (l2 >> (4 + 2 * ((mva >> 14) & 3))) & 3, true, true),
                    2 => {
                        let aps = (l2 >> 4) & 0xFF;
                        let same = aps == (aps & 3) * 0x55;
                        ((l2 & 0xFFFF_F000) | (mva & 0xFFF), (l2 >> (4 + 2 * ((mva >> 10) & 3))) & 3, true, same)
                    }
                    _ => ((l2 & 0xFFFF_FC00) | (mva & 0x3FF), (l2 >> 4) & 3, true, false),
                }
            }
        };
        let perm = match (self.dacr >> (2 * domain)) & 3 {
            0 | 2 => return Err(if page { 0xB } else { 0x9 } | domain << 4),
            3 => P_PR | P_PW | P_UR | P_UW,
            _ => match ap {
                0 => match (self.sctlr >> 8) & 3 {
                    0 => 0,
                    2 => P_PR | P_UR,
                    _ => P_PR,
                },
                1 => P_PR | P_PW,
                2 => P_PR | P_PW | P_UR,
                _ => P_PR | P_PW | P_UR | P_UW,
            },
        };
        Ok((pa, perm, page, tlbable))
    }

    /// Slow path of a guest access: translate (filling the TLB), check permissions.
    /// Err: the access faulted (abort recorded) or failed.
    fn translate(&mut self, va: u32, need: u8, fetch: bool) -> R<u32> {
        match self.walk(va) {
            Ok((pa, perm, page, tlbable)) => {
                if perm & need == 0 {
                    return Err(self.fault(va, if page { 0xF } else { 0xD }, fetch));
                }
                if tlbable {
                    let ppage = pa >> PAGE_BITS;
                    let host = if self.kind[ppage as usize] == RAM && self.rw_hooks.is_empty() {
                        self.page_ptr(pa & !0xFFF)
                    } else {
                        std::ptr::null_mut()
                    };
                    let vpn = va >> PAGE_BITS;
                    let fast = !host.is_null() && self.sctlr & 2 == 0;
                    let rtag = if fast && perm & self.rd_perm != 0 { vpn } else { NO_TAG };
                    let wtag = if fast && perm & self.wr_perm != 0 && self.code_page[ppage as usize] == 0 { vpn } else { NO_TAG };
                    self.tlb[vpn as usize & (TLB_N - 1)] = Tlb { vpn, wtag, host, pa: pa & !0xFFF, perm };
                    self.tlbf[vpn as usize & (TLB_N - 1)] = TlbFast { rtag, wtag, host };
                }
                Ok(pa)
            }
            Err(fsr) => Err(self.fault(va, fsr, fetch)),
        }
    }

    fn fault(&mut self, va: u32, fsr: u32, fetch: bool) -> Ex {
        if fetch {
            self.ifsr = fsr;
            Ex::PrefetchAbort
        } else {
            self.dfsr = fsr;
            self.far = va;
            Ex::DataAbort
        }
    }

    /// Physical access of `size` bytes (RAM / MMIO / unmapped hooks).
    fn phys_load(&mut self, va: u32, pa: u32, size: usize, fetch: bool) -> R<u32> {
        loop {
            let k = self.kind[(pa >> PAGE_BITS) as usize];
            match k {
                RAM => {
                    let p = self.page_ptr(pa & !0xFFF);
                    let off = (pa & 0xFFF) as usize;
                    let mut v = 0u32;
                    for i in 0..size {
                        let b = if off + i < 4096 {
                            unsafe { *p.add(off + i) }
                        } else {
                            let mut x = [0u8];
                            let _ = self.mem_read(pa as u64 + i as u64, &mut x);
                            x[0]
                        };
                        v |= (b as u32) << (8 * i);
                    }
                    return Ok(v);
                }
                UNMAPPED => {
                    let t = if fetch { MemType::FETCH_UNMAPPED } else { MemType::READ_UNMAPPED };
                    if !self.unmapped(t, pa as u64, size, 0) {
                        return Err(Ex::Err(if fetch { uc_error::FETCH_UNMAPPED } else { uc_error::READ_UNMAPPED }));
                    }
                }
                _ => {
                    let m = &self.mmio[(k - 2) as usize];
                    let (base, cb) = (m.base, m.read.clone());
                    let _ = va;
                    return Ok(match cb {
                        Some(cb) => (cb.borrow_mut())(self, pa as u64 - base, size) as u32,
                        None => 0,
                    });
                }
            }
        }
    }

    fn phys_store(&mut self, pa: u32, size: usize, v: u32) -> R<()> {
        loop {
            let k = self.kind[(pa >> PAGE_BITS) as usize];
            match k {
                RAM => {
                    let bytes = v.to_le_bytes();
                    let _ = self.mem_write(pa as u64, &bytes[..size]);
                    return Ok(());
                }
                UNMAPPED => {
                    if !self.unmapped(MemType::WRITE_UNMAPPED, pa as u64, size, v as i64) {
                        return Err(Ex::Err(uc_error::WRITE_UNMAPPED));
                    }
                }
                _ => {
                    let m = &self.mmio[(k - 2) as usize];
                    let (base, cb) = (m.base, m.write.clone());
                    if let Some(cb) = cb {
                        (cb.borrow_mut())(self, pa as u64 - base, size, v as u64);
                    }
                    return Ok(());
                }
            }
        }
    }

    fn unmapped(&mut self, t: MemType, addr: u64, size: usize, v: i64) -> bool {
        let hooks = self.unmapped_hooks.clone();
        for h in hooks {
            if (h.borrow_mut())(self, t, addr, size, v) {
                self.flush_tlb();
                return self.kind[(addr >> PAGE_BITS) as usize] != UNMAPPED;
            }
        }
        false
    }

    fn rw_hook(&mut self, t: MemType, va: u32, size: usize, v: u32) {
        let bit = if t == MemType::WRITE { HookType::MEM_WRITE.0 } else { HookType::MEM_READ.0 };
        let hs: Vec<MemCb<'a>> = self.rw_hooks.iter()
            .filter(|(ty, b, e, _)| ty & bit != 0 && (*b..=*e).contains(&(va as u64)))
            .map(|h| h.3.clone()).collect();
        for h in hs {
            (h.borrow_mut())(self, t, va as u64, size, v as i64);
        }
    }

    fn check_align(&mut self, va: u32, size: u32) -> R<()> {
        if self.sctlr & 2 != 0 && va & (size - 1) != 0 {
            return Err(self.fault(va, 0x1, false));
        }
        Ok(())
    }

    fn load_slow(&mut self, va: u32, size: usize, perm: u8) -> R<u32> {
        self.stat_slow += 1;
        // translated before (MMIO pages, typically): no table walk
        let e = self.tlb[(va >> PAGE_BITS) as usize & (TLB_N - 1)];
        if e.vpn == va >> PAGE_BITS && e.perm & perm != 0 && self.sctlr & 2 == 0
            && (va & 0xFFF) as usize + size <= 4096 && self.rw_hooks.is_empty()
        {
            return self.phys_load(va, e.pa | (va & 0xFFF), size, false);
        }
        self.check_align(va, size as u32)?;
        if (va & 0xFFF) as usize + size > 4096 {
            let mut v = 0;
            for i in 0..size as u32 {
                v |= self.load_slow(va.wrapping_add(i), 1, perm)? << (8 * i);
            }
            return Ok(v);
        }
        let pa = self.translate(va, perm, false)?;
        let v = self.phys_load(va, pa, size, false)?;
        if !self.rw_hooks.is_empty() {
            self.rw_hook(MemType::READ, va, size, v);
        }
        Ok(v)
    }

    fn store_slow(&mut self, va: u32, size: usize, v: u32, perm: u8) -> R<()> {
        let e = self.tlb[(va >> PAGE_BITS) as usize & (TLB_N - 1)];
        if e.vpn == va >> PAGE_BITS && e.perm & perm != 0 && !e.host.is_null() && self.sctlr & 2 == 0
            && (va & 0xFFF) as usize + size <= 4096 && self.rw_hooks.is_empty()
        {
            let pa = e.pa | (va & 0xFFF);
            let b = v.to_le_bytes();
            unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), e.host.add((va & 0xFFF) as usize), size) };
            self.smc_check(pa);
            if size == 4 && (pa & 0xFF) > 252 {
                self.smc_check(pa + 3);
            }
            return Ok(());
        }
        if e.vpn == va >> PAGE_BITS && e.perm & perm != 0 && self.sctlr & 2 == 0
            && (va & 0xFFF) as usize + size <= 4096 && self.rw_hooks.is_empty()
        {
            return self.phys_store(e.pa | (va & 0xFFF), size, v);
        }
        self.check_align(va, size as u32)?;
        if (va & 0xFFF) as usize + size > 4096 {
            for i in 0..size as u32 {
                self.store_slow(va.wrapping_add(i), 1, v >> (8 * i), perm)?;
            }
            return Ok(());
        }
        let pa = self.translate(va, perm, false)?;
        if !self.rw_hooks.is_empty() {
            self.rw_hook(MemType::WRITE, va, size, v);
        }
        self.phys_store(pa, size, v)
    }

    #[inline(always)]
    fn rd_fast(&self, va: u32, size: u32) -> Option<*mut u8> {
        let e = unsafe { self.tlbf.get_unchecked((va >> PAGE_BITS) as usize & (TLB_N - 1)) };
        if e.rtag == va >> PAGE_BITS && (va & 0xFFF) <= 4096 - size {
            Some(unsafe { e.host.add((va & 0xFFF) as usize) })
        } else {
            None
        }
    }

    #[inline(always)]
    fn wr_fast(&self, va: u32, size: u32) -> Option<*mut u8> {
        let e = unsafe { self.tlbf.get_unchecked((va >> PAGE_BITS) as usize & (TLB_N - 1)) };
        if e.wtag == va >> PAGE_BITS && (va & 0xFFF) <= 4096 - size {
            Some(unsafe { e.host.add((va & 0xFFF) as usize) })
        } else {
            None
        }
    }

    #[inline(always)]
    pub(crate) fn ld32(&mut self, va: u32) -> R<u32> {
        match self.rd_fast(va, 4) {
            Some(p) => Ok(u32::from_le(unsafe { (p as *const u32).read_unaligned() })),
            None => self.load_slow(va, 4, self.rd_perm),
        }
    }

    #[inline(always)]
    pub(crate) fn ld16(&mut self, va: u32) -> R<u32> {
        match self.rd_fast(va, 2) {
            Some(p) => Ok(u16::from_le(unsafe { (p as *const u16).read_unaligned() }) as u32),
            None => self.load_slow(va, 2, self.rd_perm),
        }
    }

    #[inline(always)]
    pub(crate) fn ld8(&mut self, va: u32) -> R<u32> {
        match self.rd_fast(va, 1) {
            Some(p) => Ok(unsafe { *p } as u32),
            None => self.load_slow(va, 1, self.rd_perm),
        }
    }

    #[inline(always)]
    pub(crate) fn st32(&mut self, va: u32, v: u32) -> R<()> {
        match self.wr_fast(va, 4) {
            Some(p) => {
                unsafe { (p as *mut u32).write_unaligned(v.to_le()) };
                Ok(())
            }
            None => self.store_slow(va, 4, v, self.wr_perm),
        }
    }

    #[inline(always)]
    pub(crate) fn st16(&mut self, va: u32, v: u32) -> R<()> {
        match self.wr_fast(va, 2) {
            Some(p) => {
                unsafe { (p as *mut u16).write_unaligned((v as u16).to_le()) };
                Ok(())
            }
            None => self.store_slow(va, 2, v, self.wr_perm),
        }
    }

    #[inline(always)]
    pub(crate) fn st8(&mut self, va: u32, v: u32) -> R<()> {
        match self.wr_fast(va, 1) {
            Some(p) => {
                unsafe { *p = v as u8 };
                Ok(())
            }
            None => self.store_slow(va, 1, v, self.wr_perm),
        }
    }

    /// Loads / stores with user permissions (LDRT / STRT).
    pub(crate) fn ld_user(&mut self, va: u32, size: usize) -> R<u32> {
        self.load_slow(va, size, P_UR)
    }

    pub(crate) fn st_user(&mut self, va: u32, size: usize, v: u32) -> R<()> {
        self.store_slow(va, size, v, P_UW)
    }

    /// Physical address of the instruction at `va` (RAM; unmapped memory goes to the
    /// unmapped hooks first).
    #[inline(always)]
    pub(super) fn fetch_pa(&mut self, va: u32) -> R<u32> {
        let i = (va >> PAGE_BITS) as usize & (TLB_N - 1);
        if unsafe { self.tlbf.get_unchecked(i) }.rtag == va >> PAGE_BITS {
            return Ok(self.tlb[i].pa | (va & 0xFFF));
        }
        let pa = self.translate(va, self.rd_perm, true)?;
        loop {
            match self.kind[(pa >> PAGE_BITS) as usize] {
                RAM => return Ok(pa),
                UNMAPPED => {
                    if !self.unmapped(MemType::FETCH_UNMAPPED, pa as u64, 4, 0) {
                        return Err(Ex::Err(uc_error::FETCH_UNMAPPED));
                    }
                }
                _ => return Err(Ex::Err(uc_error::FETCH_UNMAPPED)),
            }
        }
    }

    // ----- hooks -----------------------------------------------------------------

    fn add_hook(&mut self, h: Hook<'a>) -> UcHookId {
        self.hooks.push(Some(h));
        self.rebuild_hooks();
        self.hooks.len() - 1
    }

    pub fn remove_hook(&mut self, id: UcHookId) -> Result<(), uc_error> {
        match self.hooks.get_mut(id) {
            Some(h) => {
                *h = None;
                self.rebuild_hooks();
                Ok(())
            }
            None => Err(uc_error::ARG),
        }
    }

    fn rebuild_hooks(&mut self) {
        let mut addr: HashMap<u32, Vec<CodeCb<'a>>> = HashMap::default();
        self.all_code_hooks.clear();
        self.block_hooks.clear();
        self.intr_hooks.clear();
        self.invalid_hooks.clear();
        self.unmapped_hooks.clear();
        self.rw_hooks.clear();
        for h in self.hooks.iter().flatten() {
            match h {
                Hook::Code(b, e, cb) if b == e => addr.entry(*b as u32).or_default().push(cb.clone()),
                Hook::Code(b, e, cb) => self.all_code_hooks.push((*b, *e, cb.clone())),
                Hook::Block(cb) => self.block_hooks.push(cb.clone()),
                Hook::Intr(cb) => self.intr_hooks.push(cb.clone()),
                Hook::Invalid(cb) => self.invalid_hooks.push(cb.clone()),
                Hook::Mem(t, b, e, cb) => {
                    if t & HookType::MEM_UNMAPPED.0 != 0 {
                        self.unmapped_hooks.push(cb.clone());
                    }
                    if t & (HookType::MEM_READ.0 | HookType::MEM_WRITE.0) != 0 {
                        self.rw_hooks.push((*t, *b, *e, cb.clone()));
                    }
                }
            }
        }
        self.hook_l1.fill(0);
        self.hook_bits.truncate(1);
        for &a in addr.keys() {
            let p = (a >> PAGE_BITS) as usize;
            if self.hook_l1[p] == 0 {
                self.hook_bits.push([0; 32]);
                self.hook_l1[p] = self.hook_bits.len() as u32 - 1;
            }
            let i = ((a & 0xFFF) >> 1) as usize;
            self.hook_bits[self.hook_l1[p] as usize][i >> 6] |= 1 << (i & 63);
        }
        self.addr_hooks = addr.into_iter().map(|(k, v)| (k, Rc::from(v))).collect();
        self.flush_tlb(); // rw hooks force the slow path
        self.flush_blocks(); // hooked addresses start blocks
    }

    pub fn add_code_hook<F>(&mut self, begin: u64, end: u64, callback: F) -> Result<UcHookId, uc_error>
    where
        F: FnMut(&mut Cpu<'a>, u64, u32) + 'a,
    {
        Ok(self.add_hook(Hook::Code(begin, end, Rc::new(RefCell::new(callback)))))
    }

    pub fn add_block_hook<F>(&mut self, _begin: u64, _end: u64, callback: F) -> Result<UcHookId, uc_error>
    where
        F: FnMut(&mut Cpu<'a>, u64, u32) + 'a,
    {
        Ok(self.add_hook(Hook::Block(Rc::new(RefCell::new(callback)))))
    }

    pub fn add_intr_hook<F>(&mut self, callback: F) -> Result<UcHookId, uc_error>
    where
        F: FnMut(&mut Cpu<'a>, u32) + 'a,
    {
        Ok(self.add_hook(Hook::Intr(Rc::new(RefCell::new(callback)))))
    }

    pub fn add_insn_invalid_hook<F>(&mut self, callback: F) -> Result<UcHookId, uc_error>
    where
        F: FnMut(&mut Cpu<'a>) -> bool + 'a,
    {
        Ok(self.add_hook(Hook::Invalid(Rc::new(RefCell::new(callback)))))
    }

    pub fn add_mem_hook<F>(&mut self, t: HookType, begin: u64, end: u64, callback: F) -> Result<UcHookId, uc_error>
    where
        F: FnMut(&mut Cpu<'a>, MemType, u64, usize, i64) -> bool + 'a,
    {
        let (b, e) = if begin > end { (0, u64::MAX) } else { (begin, end) };
        Ok(self.add_hook(Hook::Mem(t.0, b, e, Rc::new(RefCell::new(callback)))))
    }

    pub fn ctl_flush_tlb(&mut self) -> Result<(), uc_error> {
        self.flush_tlb();
        Ok(())
    }

    pub fn ctl_remove_cache(&mut self, _begin: u64, _end: u64) -> Result<(), uc_error> {
        Ok(())
    }

    /// Exact mode: an interrupt is (or is no longer) pending. If the CPU accepts IRQs the
    /// run stops after the current instruction; else when it unmasks them.
    pub fn irq_request(&mut self, pending: bool) {
        self.irq_line = pending;
        if pending && self.cpsr & 0x80 == 0 {
            self.stop = true;
            self.brk = true;
        }
    }

    pub fn emu_stop(&mut self) -> Result<(), uc_error> {
        self.stop = true;
        self.brk = true; // ends the current block
        Ok(())
    }

    #[inline(always)]
    fn has_addr_hook(&self, pc: u32) -> bool {
        let l1 = self.hook_l1[(pc >> PAGE_BITS) as usize];
        if l1 == 0 {
            return false;
        }
        let i = ((pc & 0xFFF) >> 1) as usize;
        self.hook_bits[l1 as usize][i >> 6] & (1 << (i & 63)) != 0
    }

    /// Run the code hooks of `pc`. True if the instruction must not run (pc moved or
    /// stop requested).
    fn code_hooks(&mut self, pc: u32, size: u32) -> bool {
        if let Some(hs) = self.addr_hooks.get(&pc).cloned() {
            for h in hs.iter() {
                self.jumped = false;
                (h.borrow_mut())(self, pc as u64, size);
                if self.stop || self.pc != pc || self.jumped {
                    return true;
                }
            }
        }
        if !self.all_code_hooks.is_empty() {
            let hs: Vec<CodeCb<'a>> = self.all_code_hooks.iter()
                .filter(|(b, e, _)| b > e || (*b..=*e).contains(&(pc as u64)))
                .map(|h| h.2.clone()).collect();
            for h in hs {
                self.jumped = false;
                (h.borrow_mut())(self, pc as u64, size);
                if self.stop || self.pc != pc || self.jumped {
                    return true;
                }
            }
        }
        false
    }

    fn intr(&mut self, n: u32) {
        let hs = self.intr_hooks.clone();
        if hs.is_empty() {
            self.err = Some(uc_error::EXCEPTION);
            self.stop = true;
        }
        for h in hs {
            (h.borrow_mut())(self, n);
        }
    }
}


#[cfg(test)]
mod bench {
    use super::*;

    fn run(code: &[u16], iters: u32) -> f64 {
        let mut c = Cpu::new();
        c.mem_map(0x1000_0000, 0x10_0000, Prot::ALL).unwrap();
        let bytes: Vec<u8> = code.iter().flat_map(|h| h.to_le_bytes()).collect();
        c.mem_write(0x1000_0000, &bytes).unwrap();
        c.reg_write(RegisterARM::R0, iters as u64).unwrap();
        c.reg_write(RegisterARM::R1, 0x1008_0000).unwrap();
        let t = std::time::Instant::now();
        // the loop ends with r0 == 0 at the `b .` after it: stop by count
        c.insn_limit = u64::MAX;
        let until = 0x1000_0000 + 2 * (code.len() as u64 - 1);
        c.emu_start(0x1000_0001, until, 0, 0).unwrap();
        let n = c.icount as f64;
        n / t.elapsed().as_secs_f64() / 1e6
    }

    #[test]
    fn mips() {
        // loop: adds r2,#1; ldr r3,[r1]; str r2,[r1,#4]; lsls r4,r2,#2; subs r0,#1; bne loop; b .
        let code = [0x3201, 0x680B, 0x604A, 0x0094, 0x3801, 0xD1F9, 0xE7FE];
        println!("thumb loop: {:.0} MIPS", run(&code, 20_000_000));
    }
}
