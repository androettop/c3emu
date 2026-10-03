//! The ARM926EJ-S machine with the RM-614 memory map, the run modes (exact, turbo,
//! per-block) and the instrumentation: instruction trace, post-mortem ring buffer,
//! HW-register access log + scriptable MMIO model, stuck-loop detectors, CPU-exception
//! capture and HLE stop points.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};

use crate::fxhash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::rc::Rc;

#[cfg(feature = "disasm")]
use capstone::prelude::*;

use crate::clock::Instant;
use crate::uc::{HookType, MemType, Prot, RegisterARM, RegisterARMCP, Uc};

use crate::layout::*;

const RING: usize = 32;
const LAST_PCS: usize = 64;
const LOOP_CHECK_EVERY: u64 = 50_000;
const MAX_EVENTS: usize = 20000;
const MODE_CACHE: usize = 1 << 16;
/// Turbo mode: run-slice length (wall clock). Slices end at the IRQ-unmask points
/// (plant_unmask_stops, slice_due), i.e. before an instruction, from the emulation
/// thread. Unicorn's own timeout stops from another thread at any point: a code hook
/// that sees the request is skipped while its instruction still runs, and an exit
/// request noticed after a load/store restarts that instruction — an MMIO access then
/// happens twice. So that is only a rare backstop (Backstop).
const TURBO_SLICE: std::time::Duration = std::time::Duration::from_micros(200);
const TURBO_BACKSTOP: std::time::Duration = std::time::Duration::from_millis(50);
const MAX_UNMAPPED: usize = 64;

#[cfg(feature = "unicorn")]
unsafe extern "C" {
    fn uc_emu_stop(uc: *mut std::ffi::c_void) -> i32;
}

/// Turbo mode: one persistent thread that stops a run slice running past
/// TURBO_BACKSTOP (a loop that never reaches an IRQ-unmask point). Unicorn's own
/// emu_start timeout would create and join a thread for every slice.
/// (The interpreter backend checks the time itself: emu_start's timeout.)
#[cfg(feature = "unicorn")]
struct Backstop {
    /// ns since `epoch` + 1 when the current slice started; 0 = no slice running.
    started: std::sync::Arc<std::sync::atomic::AtomicU64>,
    quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    epoch: Instant,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "unicorn")]
impl Backstop {
    fn new(uc: *mut std::ffi::c_void) -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let epoch = Instant::now();
        let (st, q, h) = (started.clone(), quit.clone(), uc as usize);
        let thread = std::thread::spawn(move || {
            while !q.load(Relaxed) {
                std::thread::sleep(TURBO_BACKSTOP / 4);
                let t = st.load(Relaxed);
                let now = epoch.elapsed().as_nanos() as u64 + 1;
                if t != 0 && now.saturating_sub(t) > TURBO_BACKSTOP.as_nanos() as u64
                    && st.compare_exchange(t, 0, Relaxed, Relaxed).is_ok()
                {
                    // the same call Unicorn's timeout thread makes
                    unsafe { uc_emu_stop(h as *mut std::ffi::c_void) };
                }
            }
        });
        Backstop { started, quit, epoch, thread: Some(thread) }
    }

    fn begin(&self) {
        self.started.store(self.epoch.elapsed().as_nanos() as u64 + 1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Slice over; false if the backstop had to stop it.
    fn end(&self) -> bool {
        self.started.swap(0, std::sync::atomic::Ordering::Relaxed) != 0
    }
}

#[cfg(feature = "unicorn")]
impl Drop for Backstop {
    fn drop(&mut self) {
        self.quit.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What a peripheral register returns on read (from the JSON MMIO model).
#[derive(Debug, Clone)]
pub enum MmioVal {
    Const(u32),
    Cycle(Vec<u32>),
    Toggle,
    /// Free-running counter: instructions executed / N.
    Ticks(u64),
    /// Register keeps what was written (RAM) but reads back with these bits set.
    OrMask(u32),
}

/// Parse `{ "0x90010000": 5 | [1,2,3] | "toggle" | "ones" | "ticks:N" | "or:MASK" }`.
/// The built-in peripheral model (register values the OS expects at boot).
pub const DEFAULT_MMIO_MODEL: &str = include_str!("hle-soc.json");

pub fn parse_mmio_model(json: &str) -> Result<HashMap<u64, MmioVal>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let obj = v.as_object().ok_or("MMIO model must be a JSON object")?;
    let mut out = HashMap::default();
    for (k, val) in obj {
        let addr = parse_u64(k).ok_or_else(|| format!("bad address {k}"))?;
        let m = match val {
            serde_json::Value::Number(n) => MmioVal::Const(n.as_u64().unwrap_or(0) as u32),
            serde_json::Value::Array(a) => {
                MmioVal::Cycle(a.iter().map(|x| x.as_u64().unwrap_or(0) as u32).collect())
            }
            serde_json::Value::String(s) if s == "toggle" => MmioVal::Toggle,
            serde_json::Value::String(s) if s == "ones" => MmioVal::Const(0xFFFF_FFFF),
            serde_json::Value::String(s) if s.starts_with("ticks:") => MmioVal::Ticks(
                parse_u64(&s[6..]).filter(|&n| n > 0).ok_or_else(|| format!("bad {s}"))?),
            serde_json::Value::String(s) if s.starts_with("or:") => MmioVal::OrMask(
                parse_u64(&s[3..]).ok_or_else(|| format!("bad {s}"))? as u32),
            other => return Err(format!("bad model value for {k}: {other}")),
        };
        out.insert(addr, m);
    }
    Ok(out)
}

pub fn parse_u64(s: &str) -> Option<u64> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

/// ARM / Thumb disassembler (Capstone; without the `native` feature only hex).
pub struct Disasm {
    #[cfg(feature = "disasm")]
    arm: Capstone,
    #[cfg(feature = "disasm")]
    thumb: Capstone,
}

impl Default for Disasm {
    fn default() -> Self {
        Self::new()
    }
}

impl Disasm {
    #[cfg(not(feature = "disasm"))]
    pub fn new() -> Self {
        Disasm {}
    }

    #[cfg(not(feature = "disasm"))]
    pub fn one(&self, bytes: &[u8], _addr: u64, _thumb: bool) -> String {
        bytes.iter().rev().map(|b| format!("{b:02x}")).collect()
    }

    #[cfg(feature = "disasm")]
    pub fn new() -> Self {
        let mk = |m| Capstone::new().arm().mode(m).build().expect("capstone");
        Disasm { arm: mk(arch::arm::ArchMode::Arm), thumb: mk(arch::arm::ArchMode::Thumb) }
    }

    #[cfg(feature = "disasm")]
    pub fn one(&self, bytes: &[u8], addr: u64, thumb: bool) -> String {
        let cs = if thumb { &self.thumb } else { &self.arm };
        match cs.disasm_count(bytes, addr, 1) {
            Ok(insns) => insns
                .iter()
                .next()
                .map(|i| format!("{} {}", i.mnemonic().unwrap_or("?"), i.op_str().unwrap_or("")))
                .unwrap_or_else(|| "?".into()),
            Err(_) => "?".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Touch {
    pub icount: u64,
    pub region: &'static str,
    pub op: char,
    pub addr: u64,
    pub value: u64,
    pub pc: u64,
}

#[derive(Default)]
pub struct State {
    pub icount: u64,
    pub trace_n: u64,
    /// Trace starts after this many instructions.
    pub trace_from: u64,
    /// Last executed blocks: (address, thumb, size in bytes).
    pub ring: VecDeque<(u64, bool, u32)>,
    pub last_pcs: VecDeque<u64>,
    pub pc_hits: HashMap<u64, u64>,
    pub events: Vec<(u64, String)>,
    pub mmio_model: HashMap<u64, MmioVal>,
    mmio_cycle: HashMap<u64, usize>,
    pub touches: Vec<Touch>,
    touched: HashSet<(&'static str, u64)>,
    pub periph_reads: HashMap<u64, u64>,
    pub periph_writes: HashMap<u64, u64>,
    pub bootinfo_reads: BTreeMap<u64, u64>,
    pub unmapped: Vec<(u64, MemType, u64, u64)>,
    pub exceptions: Vec<(u64, u32, u64)>,
    pub hle_stops: HashMap<u64, String>,
    pub hle_calls: BTreeMap<String, u64>,
    /// DSP command ids seen on the command ring.
    pub dsp_cmds: BTreeMap<u16, u64>,
    /// Call-site patches: addr -> (instruction length, r0 to set, name).
    pub skips: HashMap<u64, (u64, u32, String)>,
    pub deliver_exceptions: bool,
    pub swi_counts: BTreeMap<u32, u64>,
    pub stop_reason: Option<String>,
    /// The block hook stops emulation once icount reaches this (time slices).
    pub slice_end: u64,
    /// Count executions per block address (hottest-blocks report).
    pub collect_hits: bool,
    /// Block coverage window (icount range) and the first icount each block ran in it.
    pub cover: Option<(u64, u64)>,
    pub covered: HashMap<u64, u64>,
    /// Addresses with an HLE / patch / stop hook (exempt from the zero-memory check).
    pub hooked: HashSet<u64>,
    /// Only record events at or after this instruction count.
    pub events_from: u64,
    pub soc: crate::soc::SocState,
    /// OS debug messages (oslog.rs): last lines and total count.
    pub oslog: VecDeque<(u64, String)>,
    pub oslog_total: u64,
    /// ISI messages sent: (resource, message id) -> count.
    pub isi_counts: BTreeMap<(u8, u8), u64>,
    pub backing: crate::bus::Backing,
    pub panel: crate::lcd::Panel,
    /// Address ranges logged access-by-access (--iolog) inside the MMIO windows.
    pub iolog: Vec<(u64, u64)>,
    pub i2c: crate::i2c::I2cState,
    pub sim: crate::sim::SimState,
    pub dmac: crate::dmac::DmacState,
    pub keypad: crate::keypad::KeypadState,
    pub gptimer: crate::gptimer::GpTimerState,
    pub gpio: crate::gpio::GpioState,
    pub rtc: crate::rtc::RtcState,
    pub audio: crate::audio::AudioState,
    /// DSP data memory words written by the MCU (dsp.rs).
    pub dsp_mem: HashMap<u16, u16>,
    /// Scripted DSP status entries (debug: --dsp-status): (icount, entry).
    pub dsp_inject: std::collections::VecDeque<(u64, [u16; 4])>,
    /// Thumb/ARM per block address (direct-mapped cache: tag, thumb). Reading CPSR on
    /// every block was the single largest cost of the block hook.
    pub mode_cache: Vec<(u64, bool)>,
    /// Real-time mode: (start instant, icount at start, clock_skip at start). icount then
    /// follows the wall clock at the real CPU rate.
    pub turbo: Option<(Instant, u64, u64)>,
    pub rt_blocks: u64,
    /// Turbo mode: slices run, and how the last one ended.
    pub turbo_slices: u64,
    pub turbo_last_stop: &'static str,
    /// Turbo mode: addresses right after the firmware's `msr cpsr_c` instructions.
    pub unmask_stops: Vec<u64>,
    /// Turbo mode: when the current run slice should end.
    pub turbo_deadline: Option<Instant>,
    /// Exact mode (interpreter): icount is the CPU's own instruction count.
    pub exact: bool,
    /// Wall time spent in the CPU (exact mode statistics).
    pub cpu_time: f64,
    loop_regs: Option<[u64; 13]>,
    sled_seen: bool,
}

impl State {
    pub fn event(&mut self, msg: String) {
        if self.icount >= self.events_from && self.events.len() < MAX_EVENTS {
            self.events.push((self.icount, msg));
        }
    }

    pub fn touch(&mut self, region: &'static str, op: char, addr: u64, value: u64, pc: u64) {
        if self.touched.insert((region, addr)) {
            self.touches.push(Touch { icount: self.icount, region, op, addr, value, pc });
        }
    }

    pub fn mmio_value(&mut self, addr: u64, current: u32) -> u32 {
        match self.mmio_model.get(&addr) {
            None => 0,
            Some(MmioVal::Const(v)) => *v,
            Some(MmioVal::Cycle(vs)) => {
                let c = self.mmio_cycle.entry(addr).or_default();
                let v = vs[*c % vs.len()];
                *c += 1;
                v
            }
            Some(MmioVal::Toggle) => {
                let c = self.mmio_cycle.entry(addr).or_default();
                *c ^= 1;
                if *c == 1 { 0xFFFF_FFFF } else { 0 }
            }
            Some(MmioVal::Ticks(n)) => (self.icount / n) as u32,
            Some(MmioVal::OrMask(m)) => current | m,
        }
    }
}

fn reg(n: i32) -> i32 {
    i32::from(RegisterARM::R0) + n // R0..R12 are contiguous ids
}

pub fn exc_name(intno: u32) -> &'static str {
    match intno {
        1 => "UNDEF",
        2 => "SWI",
        3 => "PREFETCH_ABORT",
        4 => "DATA_ABORT",
        5 => "IRQ",
        6 => "FIQ",
        7 => "BKPT",
        _ => "?",
    }
}

/// Turbo mode: end the run slice once its time is up (called from the emulation thread,
/// see TURBO_SLICE).
pub fn slice_due(uc: &mut Uc<'_>, s: &mut State) {
    if s.turbo_deadline.is_some_and(|d| Instant::now() >= d) {
        s.turbo_deadline = None;
        s.turbo_last_stop = "due";
        let _ = uc.emu_stop();
    }
}

/// Exact mode: the guest's clock is the interpreter's instruction count; bring State's
/// copy up to date before device code runs.
#[inline(always)]
fn sync_icount(_uc: &Uc<'_>, _s: &mut State) {
    #[cfg(not(feature = "unicorn"))]
    if _s.exact {
        _s.icount = _uc.icount;
    }
}

/// Exact mode: a device access may have raised (or cleared) an interrupt.
#[inline(always)]
fn irq_check(_uc: &mut Uc<'_>, _s: &State) {
    #[cfg(not(feature = "unicorn"))]
    if _s.exact {
        _uc.irq_request(crate::soc::irq_pending(_s));
    }
}

/// The body of an address hook (HLE function, call-site skip, stop point).
fn fire_hook(uc: &mut Uc<'_>, st: &Rc<RefCell<State>>,
             hle: &Rc<RefCell<HashMap<u64, (String, HleFn)>>>, addr: u64) {
    if let Some((name, f)) = hle.borrow_mut().get_mut(&addr) {
        let mut s = st.borrow_mut();
        if let Some(r0) = f(uc, &mut s) {
            *s.hle_calls.entry(name.clone()).or_default() += 1;
            let lr = uc.reg_read(RegisterARM::LR).unwrap_or(0);
            let _ = uc.reg_write(RegisterARM::R0, r0 as u64);
            let _ = uc.set_pc(lr); // bit0 selects Thumb
            return;
        }
    }
    let mut s = st.borrow_mut();
    if let Some((len, r0, name)) = s.skips.get(&addr).cloned() {
        // replace the instruction (typically a `bl`) by `r0 = value`
        *s.hle_calls.entry(name).or_default() += 1;
        let thumb = uc.reg_read(RegisterARM::CPSR).unwrap_or(0) & 0x20 != 0;
        let _ = uc.reg_write(RegisterARM::R0, r0 as u64);
        let _ = uc.set_pc((addr + len) | thumb as u64);
        return;
    }
    if let Some(name) = s.hle_stops.get(&addr).cloned().filter(|_| s.icount >= s.events_from) {
        let args: Vec<String> =
            (0..4).map(|n| format!("0x{:x}", uc.reg_read(reg(n)).unwrap_or(0))).collect();
        s.event(format!("HLE stop: reached {name} @0x{addr:08x} r0..r3={}", args.join(",")));
        s.stop_reason = Some(format!("HLE stop at {name}"));
        let _ = uc.emu_stop();
    }
}

/// An HLE function: runs instead of the guest code at its address. Some(r0): set r0
/// and return to LR (Thumb if LR bit0). None: run the original guest code instead.
pub type HleFn = Box<dyn for<'u> FnMut(&mut Uc<'u>, &mut State) -> Option<u32>>;

pub struct Machine<'a> {
    pub uc: Uc<'a>,
    pub st: Rc<RefCell<State>>,
    pub dis: Rc<Disasm>,
    hle: Rc<RefCell<HashMap<u64, (String, HleFn)>>>,
    /// The per-block bookkeeping hook .
    block_hook: Option<crate::uc::UcHookId>,
    #[cfg(feature = "unicorn")]
    backstop: Option<Backstop>,
}

impl<'a> Machine<'a> {
    pub fn new(trace_n: u64, mmio_model: HashMap<u64, MmioVal>) -> Result<Self, String> {
        let e = |x| format!("cpu: {x:?}");
        let mut uc = crate::uc::new_cpu().map_err(e)?;
        // RAM everywhere except the MMIO windows (see bus.rs), which are carved out of
        // the low 256 MB.
        let mut ram = vec![(SDRAM, SDRAM_SZ), (MCUSW1_BASE, MCUSW1_WINDOW), (HIVEC, HIVEC_SZ),
                           (BOOTROM, BOOTROM_SZ)];
        let mut lo = LOWRAM;
        let mut holes: Vec<(u64, u64)> = crate::bus::WINDOWS.iter()
            .filter(|w| w.0 < LOWRAM + LOWRAM_SZ).map(|w| (w.0, w.1)).collect();
        holes.sort();
        for (b, sz) in holes {
            if b > lo {
                ram.push((lo, b - lo));
            }
            lo = b + sz;
        }
        ram.push((lo, LOWRAM + LOWRAM_SZ - lo));
        for (base, size) in ram {
            uc.mem_map(base, size, Prot::ALL).map_err(e)?;
        }
        let st = Rc::new(RefCell::new(State { trace_n, mmio_model, ..Default::default() }));
        let dis = Rc::new(Disasm::new());
        let mut m = Machine {
            uc, st, dis, hle: Default::default(), block_hook: None,
            #[cfg(feature = "unicorn")]
            backstop: None,
        };
        m.install_hooks().map_err(e)?;
        Ok(m)
    }

    fn install_hooks(&mut self) -> Result<(), crate::uc::uc_error> {
        // --- per-block bookkeeping (cheap: no per-instruction hook in the hot path):
        //     instruction clock, post-mortem ring, zero-memory and stuck-loop detectors,
        //     time-slice end. Blocks are counted as size / instruction width.
        let st = self.st.clone();
        self.block_hook = Some(self.uc.add_block_hook(1, 0, move |uc, addr, size| {
            let mut s = st.borrow_mut();
            if s.mode_cache.is_empty() {
                s.mode_cache = vec![(u64::MAX, false); MODE_CACHE];
            }
            let slot = ((addr >> 1) as usize) & (MODE_CACHE - 1);
            let thumb = match s.mode_cache[slot] {
                (tag, t) if tag == addr => t,
                _ => {
                    let t = uc.reg_read(RegisterARM::CPSR).unwrap_or(0) & 0x20 != 0;
                    s.mode_cache[slot] = (addr, t);
                    t
                }
            };
            let before = s.icount;
            if s.turbo.is_some() {
                // real-time clock: resync from the wall clock every 64 blocks
                s.rt_blocks += 1;
                if s.rt_blocks & 63 == 0 {
                    crate::soc::sync_clock(&mut s);
                }
            } else {
                s.icount += (size as u64 / if thumb { 2 } else { 4 }).max(1);
            }
            if s.collect_hits {
                // weighted by instructions, for the profile
                *s.pc_hits.entry(addr).or_default() += s.icount - before;
            }
            if let Some((lo, hi)) = s.cover {
                if (lo..hi).contains(&before) {
                    s.covered.entry(addr).or_insert(before);
                }
            }
            if s.ring.len() == RING {
                s.ring.pop_front();
            }
            s.ring.push_back((addr, thumb, size));
            if s.last_pcs.len() == LAST_PCS {
                s.last_pcs.pop_front();
            }
            s.last_pcs.push_back(addr);
            let in_code = (MCUSW_BASE..MCUSW_BASE + 0xE0_0000).contains(&addr)
                || (MCUSW1_BASE..MCUSW1_VA_END).contains(&addr);
            if !in_code && !s.sled_seen && !s.hooked.contains(&addr) {
                let mut b = [0u8; 4];
                let n = if thumb { 2 } else { 4 };
                if uc.mem_read(addr, &mut b[..n]).is_ok() && b[..n].iter().all(|&x| x == 0) {
                    s.sled_seen = true;
                    s.event(format!("executing zero memory at 0x{addr:08x}"));
                    s.stop_reason = Some(format!("executing zero memory at 0x{addr:08x}"));
                    let _ = uc.emu_stop();
                    return;
                }
            }
            if before / LOOP_CHECK_EVERY != s.icount / LOOP_CHECK_EVERY {
                let uniq: HashSet<u64> = s.last_pcs.iter().copied().collect();
                let mut regs = [0u64; 13];
                for (n, r) in regs.iter_mut().enumerate() {
                    *r = uc.reg_read(reg(n as i32)).unwrap_or(0);
                }
                // a loop whose registers still change (memcpy, decompressor) is progress
                let moving = s.loop_regs != Some(regs);
                s.loop_regs = Some(regs);
                if uniq.len() <= 3 && !moving {
                    let mut pcs: Vec<u64> = uniq.into_iter().collect();
                    pcs.sort();
                    s.event(format!("stuck loop (registers static) at blocks {:x?}", pcs));
                    s.stop_reason = Some(format!("stuck loop at 0x{:08x}", pcs[0]));
                    let _ = uc.emu_stop();
                    return;
                }
            }
            if s.icount >= s.slice_end {
                let _ = uc.emu_stop();
            }
            crate::soc::on_block(uc, &mut s, addr, thumb);
        })?);

        // --- MMIO windows -> peripheral bus
        for &(base, size, region) in crate::bus::WINDOWS {
            let (sr, sw) = (self.st.clone(), self.st.clone());
            self.uc.mmio_map(base, size,
                Some(move |uc: &mut Uc<'_>, off: u64, sz: usize| {
                    let mut s = sr.borrow_mut();
                    sync_icount(uc, &mut s);
                    let v = crate::bus::read(uc, &mut s, region, base + off, sz) as u64;
                    irq_check(uc, &s);
                    v
                }),
                Some(move |uc: &mut Uc<'_>, off: u64, sz: usize, v: u64| {
                    let mut s = sw.borrow_mut();
                    sync_icount(uc, &mut s);
                    crate::bus::write(uc, &mut s, region, base + off, sz, v as u32);
                    irq_check(uc, &s);
                }))?;
        }

        // --- unmapped: log, then back with a RAM page so the run can continue
        let st = self.st.clone();
        self.uc.add_mem_hook(HookType::MEM_UNMAPPED, 1, 0, move |uc, t, addr, _size, _v| {
            let pc = uc.reg_read(RegisterARM::PC).unwrap_or(0);
            {
                let mut s = st.borrow_mut();
                if s.unmapped.len() < MAX_UNMAPPED {
                    let ic = s.icount;
                    s.unmapped.push((ic, t, addr, pc));
                }
            }
            uc.mem_map(addr & !0xFFF, 0x1000, Prot::ALL).is_ok()
        })?;

        // --- coprocessor registers Unicorn's ARM926 lacks (TCM region c9,c1; Jazelle):
        //     MRC reads 0, MCR is ignored; each encoding is logged once.
        let st = self.st.clone();
        self.uc.add_insn_invalid_hook(move |uc| {
            let pc = uc.reg_read(RegisterARM::PC).unwrap_or(0);
            let thumb = uc.reg_read(RegisterARM::CPSR).unwrap_or(0) & 0x20 != 0;
            let mut b = [0u8; 4];
            if uc.mem_read(pc, &mut b).is_err() {
                return false;
            }
            let insn = if thumb {
                // Thumb-2 MRC/MCR: hw1 = 0xEE?? / 0xFE??, hw2 = Rt|coproc|...
                (u16::from_le_bytes([b[0], b[1]]) as u32) << 16 | u16::from_le_bytes([b[2], b[3]]) as u32
            } else {
                u32::from_le_bytes(b)
            };
            // CP15 (TCM etc.) and CP14 (Jazelle config, opc1 = 7: the JVM runs its
            // software interpreter when Jazelle reads as absent)
            let cp = (insn >> 8) & 0xF;
            let is_cp = insn & 0x0F00_0010 == 0x0E00_0010 && (cp == 15 || cp == 14);
            if !is_cp {
                return false;
            }
            let rt = (insn >> 12) & 0xF;
            let mrc = insn & (1 << 20) != 0;
            if mrc && rt != 15 {
                let _ = uc.reg_write(reg(rt as i32), 0);
            }
            let (crn, crm, op1, op2) = ((insn >> 16) & 0xF, insn & 0xF, (insn >> 21) & 7, (insn >> 5) & 7);
            let mut s = st.borrow_mut();
            let key = format!("cp{cp} {} c{crn},c{crm},{op1},{op2}", if mrc { "mrc" } else { "mcr" });
            let first = !s.hle_calls.contains_key(&key);
            *s.hle_calls.entry(key.clone()).or_default() += 1;
            if first {
                s.event(format!("unimplemented {key} at 0x{pc:08x} ({})", if mrc { "read as 0" } else { "ignored" }));
            }
            let _ = uc.set_pc((pc + 4) | thumb as u64);
            true
        })?;

        // --- CPU exceptions: SWI/UNDEF/PABT are taken through the guest's vectors (ARM
        //     exception entry done by hand: Unicorn only reports them); anything else,
        //     or an exception with no vector installed, is recorded and stops the run.
        let (st, dis) = (self.st.clone(), self.dis.clone());
        let hle = self.hle.clone();
        self.uc.add_intr_hook(move |uc, intno| {
            let pc = uc.reg_read(RegisterARM::PC).unwrap_or(0);
            let cpsr = uc.reg_read(RegisterARM::CPSR).unwrap_or(0);
            // BKPT planted at a hooked address whose code hook did not run
            if intno == 7 && st.borrow().hooked.contains(&(pc & !1)) {
                *st.borrow_mut().hle_calls.entry(format!("bkpt fallback 0x{:08x}", pc & !1)).or_default() += 1;
                fire_hook(uc, &st, &hle, pc & !1);
                return;
            }
            let mut s = st.borrow_mut();
            let ic = s.icount;
            s.exceptions.push((ic, intno, pc));
            // MCUSW1 executes in place at 0x85xxxxxx: the OS VMM would map that window, but
            // its address-space objects are never created without the real loader/flash.
            // On the first fault there, map the window 1:1 (1 MB sections) in the OS's own
            // L1 table, flush the TLB and retry the instruction.
            if intno == 3 && XIP_WINDOWS.iter().any(|&(lo, hi)| (lo..hi).contains(&(pc & !1))) {
                let mut ttbr = RegisterARMCP { cp: 15, crn: 2, ..Default::default() };
                let _ = uc.reg_read_arm_coproc(&mut ttbr);
                let l1 = ttbr.val as u64 & 0xFFFF_C000;
                let mut mapped = 0;
                for mb in XIP_WINDOWS.iter().flat_map(|&(lo, hi)| (lo >> 20)..((hi + 0xF_FFFF) >> 20)) {
                    let desc = ((mb << 20) as u32) | XIP_SECTION_ATTR;
                    let mut cur = [0u8; 4];
                    let _ = uc.mem_read(l1 + 4 * mb, &mut cur);
                    if cur == [0; 4] {
                        let _ = uc.mem_write(l1 + 4 * mb, &desc.to_le_bytes());
                        mapped += 1;
                    }
                }
                if mapped > 0 {
                    let _ = uc.ctl_flush_tlb();
                    s.exceptions.pop();
                    s.event(format!("xip: mapped MCUSW1+PPM windows ({mapped} sections) on fault at 0x{pc:08x}"));
                    *s.hle_calls.entry("xip map MCUSW1".into()).or_default() += 1;
                    let _ = uc.set_pc(pc | ((cpsr >> 5) & 1)); // retry, same instruction set
                    return;
                }
            }
            if s.deliver_exceptions && (1..=3).contains(&intno) {
                let mut cp = RegisterARMCP { cp: 15, crn: 1, ..Default::default() };
                let _ = uc.reg_read_arm_coproc(&mut cp);
                let vbase = if cp.val & (1 << 13) != 0 { HIVEC } else { 0 };
                // (vector offset, mode, LR): UNDEF/SWI: pc = next insn already;
                // PREFETCH_ABORT: pc = faulting insn, LR_abt = pc + 4.
                let (voff, mode, lr) = match intno {
                    2 => (0x08, 0x13, pc),
                    3 => (0x0C, 0x17, (pc & !1) + 4),
                    _ => (0x04, 0x1B, pc),
                };
                if intno == 3 {
                    *s.hle_calls.entry("prefetch aborts delivered".into()).or_default() += 1;
                }
                if intno == 1 || intno == 3 {
                    let b = uc.mem_read_as_vec(pc & !1, 4).unwrap_or_default();
                    let (n, last) = (s.turbo_slices, s.turbo_last_stop);
                    let lr0 = uc.reg_read(RegisterARM::LR).unwrap_or(0);
                    let regs: Vec<String> = (0..8).map(|n| format!("r{n}={:x}", uc.reg_read(reg(n)).unwrap_or(0))).collect();
                    s.event(format!("{} delivered at pc=0x{pc:08x} cpsr=0x{cpsr:08x} lr=0x{lr0:08x} bytes={} (slice {n}, last stop {last}) {}",
                                    exc_name(intno), b.iter().map(|x| format!("{x:02x}")).collect::<String>(), regs.join(" ")));
                }
                let mut vec = [0u8; 4];
                let _ = uc.mem_read(vbase + voff, &mut vec);
                if vec != [0; 4] {
                    if intno == 2 {
                        // SWI number from the instruction just executed (pc = next insn)
                        let thumb = cpsr & 0x20 != 0;
                        let n = if thumb {
                            let mut b = [0u8; 2];
                            let _ = uc.mem_read(pc - 2, &mut b);
                            b[0] as u32
                        } else {
                            let mut b = [0u8; 4];
                            let _ = uc.mem_read(pc - 4, &mut b);
                            u32::from_le_bytes(b) & 0xFF_FFFF
                        };
                        *s.swi_counts.entry(n).or_default() += 1;
                        if s.swi_counts.values().sum::<u64>() <= 20 {
                            let r: Vec<String> = (0..4)
                                .map(|i| format!("0x{:x}", uc.reg_read(reg(i)).unwrap_or(0))).collect();
                            let lr = uc.reg_read(RegisterARM::LR).unwrap_or(0);
                            s.event(format!("SWI #0x{n:x} from 0x{:08x} r0..r3={} lr=0x{lr:08x}",
                                            pc - if thumb { 2 } else { 4 }, r.join(",")));
                        }
                    }
                    let _ = uc.reg_write(RegisterARM::CPSR, (cpsr & !0x3F) | 0x80 | mode);
                    let _ = uc.reg_write(RegisterARM::SPSR, cpsr);
                    let _ = uc.reg_write(RegisterARM::LR, lr);
                    let _ = uc.set_pc(vbase + voff);
                    return;
                }
            }
            s.event(format!("CPU exception {} at pc=0x{pc:08x} cpsr=0x{cpsr:08x}", exc_name(intno)));
            if intno == 4 {
                let mut far = RegisterARMCP { cp: 15, crn: 6, ..Default::default() };
                let mut fsr = RegisterARMCP { cp: 15, crn: 5, ..Default::default() };
                let _ = uc.reg_read_arm_coproc(&mut far);
                let _ = uc.reg_read_arm_coproc(&mut fsr);
                let regs: Vec<String> = (0..13).map(|n| format!("r{n}={:x}", uc.reg_read(reg(n)).unwrap_or(0))).collect();
                let (n, last) = (s.turbo_slices, s.turbo_last_stop);
                s.event(format!("   data abort: FAR=0x{:08x} FSR=0x{:x} slices={n} last stop={last}", far.val, fsr.val));
                s.event(format!("   {}", regs.join(" ")));
            }
            let ctx: Vec<String> = s.ring.iter().rev().take(6).rev()
                .map(|(a, th, size)| {
                    let b = uc.mem_read_as_vec(*a, *size as usize).unwrap_or_default();
                    format!("   ctx block {a:08x} {} ({} bytes) first: {}", if *th { 'T' } else { 'A' }, size,
                            dis.one(&b[..b.len().min(4)], *a, *th))
                })
                .collect();
            for c in ctx {
                s.event(c);
            }
            s.stop_reason = Some(format!("CPU exception {}", exc_name(intno)));
            let _ = uc.emu_stop();
        })?;
        Ok(())
    }

    /// Skip the `len`-byte instruction at `addr` (a call) and set r0 = `r0` instead.
    pub fn hle_skip(&mut self, addr: u64, len: u64, r0: u32, name: &str) {
        self.st.borrow_mut().skips.insert(addr, (len, r0, name.to_string()));
        self.hook_addr(addr);
    }

    /// Run `f` instead of the guest code at `addr` (see [`HleFn`]).
    pub fn hle_fn(&mut self, addr: u64, name: &str, f: HleFn) {
        self.hle.borrow_mut().insert(addr & !1, (name.to_string(), f));
        self.hook_addr(addr & !1);
    }

    /// Stop the run when execution reaches `addr` (from `events_from` on).
    pub fn hle_stop(&mut self, addr: u64, name: &str) {
        self.st.borrow_mut().hle_stops.insert(addr & !1, name.to_string());
        self.hook_addr(addr & !1);
    }

    /// Instrument one address: HLE function, call-site patch or stop point.
    fn hook_addr(&mut self, at: u64) {
        self.st.borrow_mut().hooked.insert(at);
        let (st, hle) = (self.st.clone(), self.hle.clone());
        let _ = self.uc.add_code_hook(at, at, move |uc, addr, _size| {
            sync_icount(uc, &mut st.borrow_mut());
            fire_hook(uc, &st, &hle, addr);
        });
    }

    /// Unicorn does not run the code hook of the instruction it resumes at: when a run
    /// slice ended right before a hooked address (frequent with turbo's short slices),
    /// run the hook here before resuming. Returns the (possibly redirected) start.
    fn prehook(&mut self, begin: u64) -> u64 {
        let at = begin & !1;
        if !self.st.borrow().hooked.contains(&at) {
            return begin;
        }
        *self.st.borrow_mut().hle_calls.entry(format!("prehook 0x{at:08x}")).or_default() += 1;
        fire_hook(&mut self.uc, &self.st, &self.hle, at);
        let pc = self.uc.reg_read(RegisterARM::PC).unwrap_or(at);
        if pc == at {
            return begin; // not redirected (stop point / pass-through)
        }
        let thumb = self.uc.reg_read(RegisterARM::CPSR).unwrap_or(0) & 0x20 != 0;
        // set_pc(lr) in the hook encodes the instruction set in bit 0
        if pc & 1 == 1 { pc } else { pc | thumb as u64 }
    }

    /// Turbo mode has no per-block hook, so a pending IRQ is only noticed between run
    /// slices. Code that keeps IRQs masked most of the time (the idle loop around WFI,
    /// byte-wise LCD transfers in critical sections) would then almost always be caught
    /// masked and the IRQ starves (the OS's task monitor resets the phone). So plant a
    /// code hook right after every ARM `msr cpsr_c, Rm` of the firmware (Thumb on the
    /// ARM926 cannot write CPSR): if that unmasked IRQs while one is pending, the slice
    /// ends there and the IRQ is taken. Per-address hooks cost nothing elsewhere and
    /// need no translation-cache flush (single-stepping with an instruction count does
    /// flush it, every time).
    fn plant_unmask_stops(&mut self) {
        if !self.st.borrow().unmask_stops.is_empty() {
            return;
        }
        let mut sites = Vec::new();
        for (base, len) in [(MCUSW_BASE, MCUSW1_BASE - MCUSW_BASE), (MCUSW1_BASE, MCUSW1_WINDOW)] {
            for chunk in (base..base + len).step_by(0x1_0000) {
                let Ok(mem) = self.uc.mem_read_as_vec(chunk, 0x1_0000) else { continue };
                for (i, w) in mem.chunks_exact(4).enumerate() {
                    let w = u32::from_le_bytes(w.try_into().unwrap());
                    // MSR CPSR, Rm with the control field (any condition)
                    if w & 0x0FF1_FFF0 == 0x0121_F000 && w >> 28 != 0xF {
                        sites.push(chunk + 4 * i as u64 + 4);
                    }
                }
            }
        }
        for &at in &sites {
            let st = self.st.clone();
            let _ = self.uc.add_code_hook(at, at, move |uc, _, _| {
                let cpsr = uc.reg_read(RegisterARM::CPSR).unwrap_or(0);
                let mut s = st.borrow_mut();
                if cpsr & 0x80 == 0 && crate::soc::irq_pending(&s) {
                    s.turbo_last_stop = "unmask";
                    let _ = uc.emu_stop();
                } else {
                    slice_due(uc, &mut s);
                }
            });
            let _ = self.uc.ctl_remove_cache(at, at + 4);
        }
        let mut s = self.st.borrow_mut();
        s.event(format!("turbo: {} IRQ-unmask stop points", sites.len()));
        s.unmask_stops = sites;
    }

    /// Print each executed instruction while trace_from < icount <= trace_from + n.
    pub fn enable_trace(&mut self, from: u64, n: u64) {
        let (st, dis) = (self.st.clone(), self.dis.clone());
        let mut seq = 0u64;
        let _ = self.uc.add_code_hook(1, 0, move |uc, addr, size| {
            let s = st.borrow();
            if s.icount < from || seq >= n {
                return;
            }
            seq += 1;
            let thumb = uc.reg_read(RegisterARM::CPSR).unwrap_or(0) & 0x20 != 0;
            let bytes = uc.mem_read_as_vec(addr, size as usize).unwrap_or_default();
            println!("  ~{:<10} {addr:08x} {} {}", s.icount, if thumb { 'T' } else { 'A' },
                     dis.one(&bytes, addr, thumb));
        });
    }

    /// Disassemble the last `blocks` executed blocks (post-mortem context).
    pub fn context(&self, blocks: usize) -> Vec<String> {
        let s = self.st.borrow();
        let mut out = Vec::new();
        for &(a, th, size) in s.ring.iter().rev().take(blocks).rev() {
            let bytes = self.uc.mem_read_as_vec(a, size as usize).unwrap_or_default();
            let mut off = 0usize;
            while off < bytes.len() {
                let n = if th && off + 2 <= bytes.len()
                    && u16::from_le_bytes([bytes[off], bytes[off + 1]]) >> 11 < 0b11101 { 2 } else { 4 };
                let n = n.min(bytes.len() - off);
                out.push(format!("{:08x} {} {}", a + off as u64, if th { 'T' } else { 'A' },
                                 self.dis.one(&bytes[off..off + n], a + off as u64, th)));
                off += n;
            }
        }
        out
    }

    /// Log every read and write in [addr, addr+len) as events (value + pc).
    pub fn iolog(&mut self, addr: u64, len: u64) -> Result<(), String> {
        if crate::bus::WINDOWS.iter().any(|w| addr >= w.0 && addr + len <= w.0 + w.1) {
            self.st.borrow_mut().iolog.push((addr, addr + len));
            return Ok(());
        }
        let st = self.st.clone();
        self.uc.add_mem_hook(HookType::MEM_READ | HookType::MEM_WRITE, addr, addr + len - 1,
            move |uc, t, a, size, v| {
                let pc = uc.reg_read(RegisterARM::PC).unwrap_or(0);
                let val = if t == MemType::WRITE {
                    v as u64 & 0xFFFF_FFFF
                } else {
                    let mut b = [0u8; 4];
                    let _ = uc.mem_read(a, &mut b[..size.min(4)]);
                    u32::from_le_bytes(b) as u64
                };
                let op = if t == MemType::WRITE { 'W' } else { 'R' };
                st.borrow_mut().event(format!("io {op}{size} 0x{a:08x} = 0x{val:x} pc=0x{pc:08x}"));
                true
            }).map(|_| ()).map_err(|e| format!("iolog: {e:?}"))
    }

    /// Record the first read of every address in [addr, addr+len) in the touch log
    /// (region "track"): shows which fields the OS expects someone else to fill.
    pub fn track(&mut self, addr: u64, len: u64) -> Result<(), String> {
        let st = self.st.clone();
        self.uc.add_mem_hook(HookType::MEM_READ, addr, addr + len - 1, move |uc, _t, a, size, _v| {
            let pc = uc.reg_read(RegisterARM::PC).unwrap_or(0);
            let mut b = [0u8; 4];
            let _ = uc.mem_read(a, &mut b[..size.min(4)]);
            st.borrow_mut().touch("track", 'R', a, u32::from_le_bytes(b) as u64, pc);
            true
        }).map(|_| ()).map_err(|e| format!("track: {e:?}"))
    }

    /// Log every write to [addr, addr+4) as an event (value + pc).
    pub fn watch(&mut self, addr: u64) -> Result<(), String> {
        let st = self.st.clone();
        self.uc.add_mem_hook(HookType::MEM_WRITE, addr, addr + 3, move |uc, _t, a, size, v| {
            let pc = uc.reg_read(RegisterARM::PC).unwrap_or(0);
            let mut s = st.borrow_mut();
            s.event(format!("watch: W{size} 0x{a:08x} = 0x{:x} pc=0x{pc:08x}", v as u64 & 0xFFFF_FFFF));
            true
        }).map(|_| ()).map_err(|e| format!("watch: {e:?}"))
    }

    pub fn load(&mut self, data: &[u8], base: u64) -> Result<(), String> {
        self.uc.mem_write(base, data).map_err(|e| format!("load @0x{base:x}: {e:?}"))
    }

    pub fn sctlr(&self) -> u32 {
        let mut r = RegisterARMCP { cp: 15, crn: 1, ..Default::default() };
        let _ = self.uc.reg_read_arm_coproc(&mut r);
        r.val as u32
    }

    pub fn set_sctlr(&mut self, v: u32) {
        let r = RegisterARMCP { cp: 15, crn: 1, val: v as u64, ..Default::default() };
        let _ = self.uc.reg_write_arm_coproc(&r);
    }

    pub fn reg(&self, n: i32) -> u32 {
        self.uc.reg_read(reg(n)).unwrap_or(0) as u32
    }

    /// Set the banked SP of each ARM mode, finish in `final_mode`. Returns the SPs read
    /// back per mode (proves Unicorn switched banks on the CPSR writes).
    pub fn setup_stacks(&mut self, final_mode: u32) -> Vec<(&'static str, u32)> {
        for &(_, mode, top) in MODE_STACKS {
            let _ = self.uc.reg_write(RegisterARM::CPSR, (CPSR_IF | mode) as u64);
            let _ = self.uc.reg_write(RegisterARM::SP, top);
        }
        let mut back = Vec::new();
        for &(name, mode, _) in MODE_STACKS {
            let _ = self.uc.reg_write(RegisterARM::CPSR, (CPSR_IF | mode) as u64);
            back.push((name, self.uc.reg_read(RegisterARM::SP).unwrap_or(0) as u32));
        }
        let _ = self.uc.reg_write(RegisterARM::CPSR, (CPSR_IF | final_mode) as u64);
        back
    }

    /// Run from `entry` (bit0 = Thumb) for at most `max_insns`; returns the stop reason.
    pub fn run(&mut self, entry: u64, max_insns: u64) -> String {
        self.run_with(entry, max_insns, u64::MAX, |_| true)
    }

    /// Run like `run`, returning to `tick` every `slice` instructions (e.g. to refresh a
    /// window); `tick` returning false stops the run.
    pub fn run_with(&mut self, entry: u64, max_insns: u64, slice: u64,
                    mut tick: impl FnMut(&mut Self) -> bool) -> String {
        let mut begin = entry;
        loop {
            let remaining = max_insns.saturating_sub(self.st.borrow().icount);
            if remaining == 0 {
                return format!("instruction limit ({max_insns})");
            }
            {
                let mut s = self.st.borrow_mut();
                s.slice_end = s.icount + remaining.min(slice);
            }
            begin = self.prehook(begin);
            let r = self.uc.emu_start(begin, 0xFFFF_FFF0, 0, 0);
            let pc = self.uc.reg_read(RegisterARM::PC).unwrap_or(0);
            {
                let mut s = self.st.borrow_mut();
                if let Some(why) = &s.stop_reason {
                    return why.clone();
                }
                if let Err(e) = r {
                    return format!("UcError {e:?} @pc=0x{pc:08x}");
                }
                if s.icount >= max_insns {
                    return format!("instruction limit ({max_insns})");
                }
                // Halted on WFI (ARM926: mcr p15,0,Rd,c7,c0,4) -> idle until the next event.
                let prev = crate::mmu::virt_word(&self.uc, (pc as u32).wrapping_sub(4)).unwrap_or(0);
                if prev & 0x0FFF_0FFF == 0x0E07_0F90 {
                    crate::soc::on_wfi(&mut s);
                }
            }
            if !tick(self) {
                return "stopped by the user".into();
            }
            let thumb = self.uc.reg_read(RegisterARM::CPSR).unwrap_or(0) & 0x20 != 0;
            begin = pc | thumb as u64;
        }
    }

    /// Real-time mode (live window): the phone's clock follows the wall clock instead
    /// of the instruction count. When the emulator is slower than the real CPU, the OS
    /// still sees real time pass, so its high-priority tasks (audio) keep up and the
    /// low-priority ones (animations) drop frames, as on a loaded phone.
    pub fn run_realtime(&mut self, entry: u64, max_insns: u64, tick: impl FnMut(&mut Self) -> bool) -> String {
        {
            let mut s = self.st.borrow_mut();
            let base = s.icount;
            s.turbo = Some((Instant::now(), base, s.soc.clock_skip));
        }
        self.run_with(entry, max_insns, 2_000_000, tick)
    }

    /// Turbo mode (live window): Unicorn runs free, without the per-block hook, in
    /// short wall-clock slices (TURBO_SLICE). The phone's clock is the wall clock (as in
    /// run_realtime); between slices the devices are updated and a pending IRQ is taken
    /// if the CPU accepts it — else at the next point where the firmware unmasks IRQs
    /// (plant_unmask_stops).
    pub fn run_turbo(&mut self, entry: u64, max_insns: u64, mut tick: impl FnMut(&mut Self) -> bool) -> String {
        self.turbo_start(entry);
        loop {
            if let Err(why) = self.turbo_slice(max_insns) {
                return why;
            }
            if !tick(self) {
                return "stopped by the user".into();
            }
        }
    }

    /// Enter turbo mode at `entry` (see run_turbo); then call turbo_slice repeatedly.
    pub fn turbo_start(&mut self, entry: u64) {
        if let Some(h) = self.block_hook.take() {
            let _ = self.uc.remove_hook(h);
        }
        {
            let mut s = self.st.borrow_mut();
            let base = s.icount;
            s.turbo = Some((Instant::now(), base, s.soc.clock_skip));
        }
        self.plant_unmask_stops();
        #[cfg(feature = "unicorn")]
        {
            self.backstop = Some(Backstop::new(self.uc.get_handle() as *mut std::ffi::c_void));
        }
        let _ = self.uc.set_pc(entry);
    }

    /// One turbo run slice (about TURBO_SLICE of wall time) plus the device work and
    /// IRQ delivery after it. Err: the run stopped (reason).
    pub fn turbo_slice(&mut self, max_insns: u64) -> Result<(), String> {
        let pc = self.uc.reg_read(RegisterARM::PC).unwrap_or(0);
        let thumb = self.uc.reg_read(RegisterARM::CPSR).unwrap_or(0) & 0x20 != 0;
        let begin = self.prehook(pc | thumb as u64);
        if let Some(why) = self.st.borrow().stop_reason.clone() {
            return Err(why);
        }
        self.st.borrow_mut().turbo_deadline = Some(Instant::now() + TURBO_SLICE);
        #[cfg(feature = "unicorn")]
        let (r, ended) = {
            let b = self.backstop.as_ref().expect("turbo_start");
            b.begin();
            let r = self.uc.emu_start(begin, 0xFFFF_FFF0, 0, 0);
            (r, self.backstop.as_ref().unwrap().end())
        };
        #[cfg(not(feature = "unicorn"))]
        let (r, ended) = (self.uc.emu_start(begin, 0xFFFF_FFF0, TURBO_BACKSTOP.as_micros() as u64, 0), true);
        self.st.borrow_mut().turbo_deadline = None;
        let pc = self.uc.reg_read(RegisterARM::PC).unwrap_or(0);
        let cpsr = self.uc.reg_read(RegisterARM::CPSR).unwrap_or(0);
        {
            let mut s = self.st.borrow_mut();
            s.turbo_slices += 1;
            crate::soc::sync_clock(&mut s);
            if let Some(why) = &s.stop_reason {
                return Err(why.clone());
            }
            if let Err(e) = r {
                return Err(format!("UcError {e:?} @pc=0x{pc:08x}"));
            }
            if s.icount >= max_insns {
                return Err(format!("instruction limit ({max_insns})"));
            }
            let prev = crate::mmu::virt_word(&self.uc, (pc as u32).wrapping_sub(4)).unwrap_or(0);
            if s.turbo_last_stop.is_empty() && prev & 0x0FFF_0FFF == 0x0E07_0F90 {
                s.turbo_last_stop = "wfi";
                crate::soc::on_wfi(&mut s);
            } else if s.turbo_last_stop.is_empty() {
                s.turbo_last_stop = if ended { "other" } else { "backstop" };
            }
            crate::soc::devices(&mut self.uc, &mut s);
        }
        // A pending IRQ is taken now if the CPU accepts it, else at a later slice
        // boundary or unmask stop point (plant_unmask_stops).
        if crate::soc::irq_pending(&self.st.borrow()) {
            let mut s = self.st.borrow_mut();
            crate::soc::take_irq(&mut self.uc, &mut s, pc | ((cpsr >> 5) & 1));
        }
        self.st.borrow_mut().turbo_last_stop = "";
        Ok(())
    }

    /// The fastest deterministic way to run the phone on this backend: exact mode on
    /// the interpreter (see exact_slice), turbo mode on Unicorn (wall clock).
    pub fn fast_start(&mut self, entry: u64) {
        #[cfg(not(feature = "unicorn"))]
        self.exact_start(entry);
        #[cfg(feature = "unicorn")]
        self.turbo_start(entry);
    }

    pub fn fast_slice(&mut self, max_insns: u64) -> Result<(), String> {
        #[cfg(not(feature = "unicorn"))]
        return self.exact_slice(max_insns);
        #[cfg(feature = "unicorn")]
        return self.turbo_slice(max_insns);
    }

    pub fn run_fast(&mut self, entry: u64, max_insns: u64, mut tick: impl FnMut(&mut Self) -> bool) -> String {
        self.fast_start(entry);
        loop {
            if let Err(why) = self.fast_slice(max_insns) {
                return why;
            }
            if !tick(self) {
                return "stopped by the user".into();
            }
        }
    }

    /// Exact mode (interpreter backend): no per-block hook and no wall clock. The
    /// interpreter counts instructions exactly; the phone's clock is that count (208 M
    /// per second, as on hardware) plus the idle time skipped at WFI. The CPU runs up to
    /// the next point where a device can change (timer tick), the devices are updated
    /// there, and an interrupt is taken at the exact instruction where it becomes
    /// pending and enabled (device accesses and CPSR writes end the run then). The run
    /// is a pure function of the inputs; it is paced to real time only by sleeping at
    /// WFI while ahead of the wall clock. (when State::soc.realtime is set: live window, web).
    #[cfg(not(feature = "unicorn"))]
    pub fn exact_start(&mut self, entry: u64) {
        if let Some(h) = self.block_hook.take() {
            let _ = self.uc.remove_hook(h);
        }
        let mut s = self.st.borrow_mut();
        s.exact = true;
        s.turbo = None;
        self.uc.icount = s.icount;
        drop(s);
        let _ = self.uc.set_pc(entry);
    }

    /// One exact-mode run up to the next device event (at most one timer tick).
    #[cfg(not(feature = "unicorn"))]
    pub fn exact_slice(&mut self, max_insns: u64) -> Result<(), String> {
        let limit = {
            let mut s = self.st.borrow_mut();
            s.icount = self.uc.icount;
            if let Some(why) = &s.stop_reason {
                return Err(why.clone());
            }
            if s.icount >= max_insns {
                return Err(format!("instruction limit ({max_insns})"));
            }
            let next = crate::soc::exact_devices(&mut self.uc, &mut s);
            if crate::soc::irq_pending(&s) {
                let pc = self.uc.reg_read(RegisterARM::PC).unwrap_or(0);
                let thumb = self.uc.reg_read(RegisterARM::CPSR).unwrap_or(0) & 0x20 != 0;
                crate::soc::take_irq(&mut self.uc, &mut s, pc | thumb as u64);
            }
            let pending = crate::soc::irq_pending(&s);
            self.uc.irq_request(pending);
            next.min(max_insns)
        };
        let pc = self.uc.reg_read(RegisterARM::PC).unwrap_or(0);
        let thumb = self.uc.reg_read(RegisterARM::CPSR).unwrap_or(0) & 0x20 != 0;
        let begin = self.prehook(pc | thumb as u64);
        if let Some(why) = self.st.borrow().stop_reason.clone() {
            return Err(why);
        }
        let t_cpu = Instant::now();
        self.uc.insn_limit = limit;
        let r = self.uc.emu_start(begin, 0xFFFF_FFF0, 0, 0);
        self.st.borrow_mut().cpu_time += t_cpu.elapsed().as_secs_f64();
        self.uc.insn_limit = u64::MAX;
        let pc = self.uc.reg_read(RegisterARM::PC).unwrap_or(0);
        let mut s = self.st.borrow_mut();
        s.icount = self.uc.icount;
        s.turbo_slices += 1;
        if let Some(why) = &s.stop_reason {
            return Err(why.clone());
        }
        if let Err(e) = r {
            return Err(format!("UcError {e:?} @pc=0x{pc:08x}"));
        }
        // halted on WFI (the previous instruction): idle until the next event
        if self.uc.wfi {
            crate::soc::on_wfi(&mut s);
        } else if s.soc.realtime.is_some() {
            // busy code (no WFI) is paced too: never ahead of real time
            crate::soc::pace(&mut s);
        }
        Ok(())
    }

    pub fn report(&self) {
        let s = self.st.borrow();
        #[cfg(not(feature = "unicorn"))]
        println!("[emu] interpreter: {} chained, {} code overwrites, {} slow loads, {} blocks run, {} decoded, {:.2} insns/block",
                 self.uc.stat_chained, self.uc.stat_smc, self.uc.stat_slow, self.uc.stat_blocks, self.uc.stat_decodes, self.uc.icount as f64 / self.uc.stat_blocks.max(1) as f64);
        #[cfg(not(feature = "unicorn"))]
        println!("[emu] time in the CPU: {:.2} s ({:.0} MIPS), slices {}", s.cpu_time, self.uc.icount as f64 / s.cpu_time.max(1e-9) / 1e6, s.turbo_slices);
        let pc = self.uc.reg_read(RegisterARM::PC).unwrap_or(0);
        let cpsr = self.uc.reg_read(RegisterARM::CPSR).unwrap_or(0);
        let sp = self.uc.reg_read(RegisterARM::SP).unwrap_or(0);
        let lr = self.uc.reg_read(RegisterARM::LR).unwrap_or(0);
        println!("[emu] executed {} insns; pc=0x{pc:08x} cpsr=0x{cpsr:08x} sp=0x{sp:08x} lr=0x{lr:08x}",
                 s.icount);
        let regs: Vec<String> = (0..13).map(|n| format!("r{n}=0x{:08x}", self.reg(n))).collect();
        println!("      {}", regs.join(" "));
        println!("[emu] SCTLR=0x{:08x}", self.sctlr());
        let vbase = if self.sctlr() & (1 << 13) != 0 { HIVEC } else { 0 };
        let vec = self.uc.mem_read_as_vec(vbase, 0x40).unwrap_or_default();
        let words: Vec<String> = vec.chunks(4).map(|c| format!("{:08x}", u32::from_le_bytes(c.try_into().unwrap()))).collect();
        println!("[emu] vectors @0x{vbase:08x}: {}", words.join(" "));
        if !s.events.is_empty() {
            println!("[emu] events:");
            for (ic, m) in &s.events {
                println!("    @{ic:>9}  {m}");
            }
        }
        println!("[emu] last instructions (last blocks):");
        drop(s);
        for l in self.context(3).iter().rev().take(24).rev() {
            println!("    {l}");
        }
        let s = self.st.borrow();
        if !s.unmapped.is_empty() {
            println!("[emu] unmapped accesses (backed with RAM pages):");
            for (ic, t, a, pc) in s.unmapped.iter().take(16) {
                println!("    @{ic:>9} {t:?} 0x{a:08x} pc=0x{pc:08x}");
            }
        }
        if !s.soc.irqs_taken.is_empty() || s.soc.wfi_count > 0 {
            let v: Vec<String> = s.soc.irqs_taken.iter().map(|(n, c)| format!("irq{n} x{c}")).collect();
            println!("[emu] IRQs taken: {} | WFI idles: {} (idle DSP frames {}) | timer now={} compare={} armed={}",
                     v.join(", "), s.soc.wfi_count, s.soc.idle_frames, crate::soc::now(&s),
                     s.soc.timer_compare, s.soc.timer_armed);
            let (ic, pc, v, n) = s.soc.last_compare;
            println!("[emu] last compare write @{ic} pc=0x{pc:08x} value={v} (now was {n}); last timer IRQ @{}",
                     s.soc.last_timer_irq);
        }
        if !s.swi_counts.is_empty() {
            let v: Vec<String> = s.swi_counts.iter().map(|(n, c)| format!("svc#0x{n:x} x{c}")).collect();
            println!("[emu] SWIs delivered: {}", v.join(", "));
        }
        if !s.i2c.slaves.is_empty() {
            let v: Vec<String> = s.i2c.slaves.iter().map(|((b, a), n)| format!("i2c{b}:0x{a:02x}x{n}")).collect();
            println!("[emu] I2C slaves addressed: {}", v.join(" "));
            let mut acc: Vec<_> = s.i2c.accesses.iter().collect();
            acc.sort_by(|a, b| b.1.cmp(a.1));
            let v: Vec<String> = acc.iter().take(16)
                .map(|((b, a, r, op), n)| format!("i2c{b}:{a:02x}[{r:02x}]{op}x{n}")).collect();
            println!("[emu] I2C register accesses: {}", v.join(" "));
        }
        {
            let m: Vec<String> = (0x0Bu8..=0x14).map(|r| format!("{:02x}", s.i2c.regs.get(&(1, crate::i2c::PMU, r)).copied().unwrap_or(0))).collect();
            println!("[emu] PMU interrupt masks INT1M..INT10M (0x0B..0x14): {}", m.join(" "));
        }
        if !s.dsp_cmds.is_empty() {
            let v: Vec<String> = s.dsp_cmds.iter().map(|(c, n)| format!("0x{c:x}x{n}")).collect();
            println!("[emu] DSP commands: {}", v.join(" "));
        }
        if !s.hle_calls.is_empty() {
            let v: Vec<String> = s.hle_calls.iter().map(|(n, c)| format!("{n} x{c}")).collect();
            println!("[emu] HLE calls: {}", v.join(", "));
        }
        if !s.bootinfo_reads.is_empty() {
            let v: Vec<String> = s.bootinfo_reads.iter().map(|(o, c)| format!("+0x{o:x}x{c}")).collect();
            println!("[emu] boot-info (r2 block) offsets read: {}", v.join(", "));
        }
        let mut hot: Vec<_> = s.pc_hits.iter().collect();
        if hot.is_empty() {
            println!("[emu] hottest blocks: (run with --hot)");
        }
        hot.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        if !hot.is_empty() {
            println!("[emu] hottest blocks:");
        }
        for (a, c) in hot.iter().take(40) {
            println!("    0x{a:08x} x{c}");
        }
        println!("[emu] HW registers touched ({}), first-touch order:", s.touches.len());
        for t in s.touches.iter().take(400) {
            println!("    @{:>9} {:6} {} 0x{:08x} = 0x{:08x}  pc=0x{:08x}",
                     t.icount, t.region, t.op, t.addr, t.value, t.pc);
        }
    }
}
