//! RM-614 SoC core devices the OS scheduler needs: the OS timer, the interrupt
//! controller, IRQ delivery, and WFI (idle) handling.
//!
//! Timer @0x0880_0000 (OS tick source, IRQ line 0):
//!   +0x10  free-running counter (read twice until stable by get_timer @0x80827C10)
//!   +0x0C  compare: the OS writes `now + delta` (0x8069A508) and re-reads it back.
//!          A write arms it; reaching it latches line 0 (edge). A freshly written
//!          compare cannot match for TIMER_SYNC_INSNS: the tick handler acks the INTC
//!          ~38 instructions *after* writing the next compare, and a match landing in
//!          between would be wiped by that ack (the OS would never get another tick).
//!          On hardware the counter ticks far slower than that window (cross-clock-
//!          domain sync), so this only removes an artifact of our coarse clock.
//! INTC @0x0881_0000, three banks (+0x000 lines 0-31, +0x100 32-63, +0x180 64-95):
//!   +0x00  enable mask (1 = enabled; init writes 0, enable = orr @0x8081839A,
//!          disable = bic @0x808183D4)
//!   +0x08  acknowledge, write-1-to-clear (0x80818370)
//!   +0x0C  pending (dispatcher @0x80818620 tests it against its line table)
//! The clock rate is not known; TIMER_INSNS_PER_TICK is a placeholder.

use std::collections::BTreeMap;

use crate::uc::{RegisterARM, RegisterARMCP, Uc};

use crate::layout::*;
use crate::machine::State;

#[derive(Default)]
pub struct SocState {
    /// Raw (unmasked) pending bits per INTC bank.
    pub intc_raw: [u32; 3],
    /// INTC enable masks (bank +0x00), cached from writes.
    pub intc_enable: [u32; 3],
    pub timer_compare: u32,
    pub timer_armed: bool,
    /// icount of the last compare write (for TIMER_SYNC_INSNS).
    pub compare_written_at: u64,
    /// Ticks added by fast-forwarding through WFI.
    pub clock_skip: u64,
    pub irqs_taken: BTreeMap<u32, u64>,
    pub wfi_count: u64,
    /// DSP frame interrupts generated while the MCU was idle.
    pub idle_frames: u64,
    /// Last compare programming: (icount, pc, value, timer now).
    pub last_compare: (u64, u64, u32, u32),
    /// icount of the last timer IRQ delivered.
    pub last_timer_irq: u64,
    /// Real-time pacing (live window): host instant and timer tick at the start. Idle
    /// fast-forward then never runs the phone's clock ahead of the wall clock.
    pub realtime: Option<(crate::clock::Instant, u32)>,
    /// Tick at which the per-tick device work last ran (on_block).
    pub last_tick: u32,
    /// Tick of the next TDMA frame interrupt (0 = not started).
    pub next_frame: u32,
    /// TDMA frame interrupts raised.
    pub frames: u64,
    /// Ticks the phone's clock jumped to catch up with the wall clock (pace).
    pub catch_up: u64,
}

/// OS timer rate (ticks per second).
pub const TIMER_HZ: u64 = 5120;

/// Real-time pacing (live window, web): the phone's clock is kept on the wall clock.
/// Ahead (the emulator is faster than the 208 MHz part): sleep. Behind (it is slower —
/// e.g. while the OS decodes music it needs the whole CPU): the phone's time jumps
/// forward to the wall clock, as on a phone with a slower CPU. So sound (the DSP plays
/// at the phone's time) and clocks stay real-time and the UI drops frames, instead of
/// everything running in slow motion. Runs that keep up are not affected (they stay
/// deterministic); unpaced runs (headless) are never affected.
pub fn pace(s: &mut State) {
    let Some((t0, tick0)) = s.soc.realtime else { return };
    let emu = std::time::Duration::from_micros(now(s).wrapping_sub(tick0) as u64 * 1_000_000 / TIMER_HZ);
    let wall = t0.elapsed();
    if emu > wall {
        crate::clock::sleep((emu - wall).min(std::time::Duration::from_millis(100)));
    } else if wall - emu > std::time::Duration::from_secs(1) {
        // a long stall of the host (suspended page, debugger): don't replay it
        s.soc.realtime = Some((crate::clock::Instant::now(), now(s)));
    } else if wall - emu > std::time::Duration::from_millis(5) {
        let lag = ((wall - emu).as_micros() as u64 * TIMER_HZ / 1_000_000).max(1);
        s.soc.clock_skip += lag;
        s.soc.compare_written_at = 0; // time passed: a fresh compare's sync window is over
        s.soc.catch_up += lag;
    }
}

pub fn now(s: &State) -> u32 {
    (s.icount / TIMER_INSNS_PER_TICK + s.soc.clock_skip) as u32
}

fn enable_mask(s: &State, bank: usize) -> u32 {
    s.soc.intc_enable[bank]
}

/// Bus read: timer counter, INTC pending.
pub fn read(s: &mut State, addr: u64) -> Option<u32> {
    if addr == TIMER_COUNTER {
        sync_clock(s);
        return Some(now(s));
    }
    let bank = INTC_BANKS.iter().position(|&b| addr == b + INTC_PENDING)?;
    update_timer(s);
    Some(raw(s, bank) & enable_mask(s, bank))
}

/// Bus write: timer compare, INTC enable / acknowledge.
pub fn write(s: &mut State, addr: u64, val: u32) {
    if addr == TIMER_COMPARE {
        s.soc.timer_compare = val;
        s.soc.timer_armed = true;
        s.soc.compare_written_at = s.icount;
        s.soc.last_compare = (s.icount, 0, val, now(s));
        return;
    }
    for (bank, &b) in INTC_BANKS.iter().enumerate() {
        if addr == b {
            s.soc.intc_enable[bank] = val;
        } else if addr == b + INTC_ACK {
            s.soc.intc_raw[bank] &= !val;
        }
    }
}

fn update_timer(s: &mut State) {
    let n = now(s);
    if s.soc.timer_armed
        && s.icount >= s.soc.compare_written_at + TIMER_SYNC_INSNS
        && (n.wrapping_sub(s.soc.timer_compare) as i32) >= 0
    {
        s.soc.timer_armed = false;
        s.soc.intc_raw[0] |= 1 << TIMER_IRQ;
    }
}

/// Pending lines of a bank before masking.
fn raw(s: &State, bank: usize) -> u32 {
    let level = if bank == 0 { crate::sim::irq_level(s) | crate::dmac::irq_level(s) } else { 0 };
    s.soc.intc_raw[bank] | level
}

/// Take the IRQ exception if a line is pending+enabled and CPSR.I is clear.
/// `pc` is the next instruction to execute. Returns true if delivered.
fn maybe_take_irq(uc: &mut Uc<'_>, s: &mut State, pc: u64) -> bool {
    let cpsr = uc.reg_read(RegisterARM::CPSR).unwrap_or(0);
    if cpsr & 0x80 != 0 {
        return false;
    }
    let mut line = None;
    for bank in 0..3 {
        let p = raw(s, bank) & enable_mask(s, bank);
        if p != 0 {
            line = Some(32 * bank as u32 + p.trailing_zeros());
            break;
        }
    }
    let Some(line) = line else { return false };
    *s.soc.irqs_taken.entry(line).or_default() += 1;
    if line == TIMER_IRQ {
        s.soc.last_timer_irq = s.icount;
    }
    let mut cp = RegisterARMCP { cp: 15, crn: 1, ..Default::default() };
    let _ = uc.reg_read_arm_coproc(&mut cp);
    let vbase = if cp.val & (1 << 13) != 0 { HIVEC } else { 0 };
    // ARM IRQ entry: mode IRQ, I set, ARM state; LR_irq = next insn + 4; SPSR = CPSR.
    let _ = uc.reg_write(RegisterARM::CPSR, (cpsr & !0x3F) | 0x80 | 0x12);
    let _ = uc.reg_write(RegisterARM::SPSR, cpsr);
    let _ = uc.reg_write(RegisterARM::LR, (pc & !1) + 4);
    let _ = uc.set_pc(vbase + 0x18);
    true
}

/// Per-block step (called from the machine's block hook): advance the timer and take
/// a pending IRQ before the block runs.
/// Real-time mode: icount follows the wall clock (208 M instructions per second).
pub fn sync_clock(s: &mut State) {
    if let Some((t0, base, _)) = s.turbo {
        s.icount = base + (t0.elapsed().as_nanos() as u64 * (TIMER_INSNS_PER_TICK * TIMER_HZ) / 1_000_000_000);
    }
}

/// An interrupt line is pending and enabled at the INTC.
pub fn irq_pending(s: &State) -> bool {
    (0..3).any(|b| raw(s, b) & enable_mask(s, b) != 0)
}

/// Turbo mode: the per-tick device work (on_block without the IRQ part).
pub fn devices(uc: &mut Uc<'_>, s: &mut State) {
    s.soc.last_tick = now(s);
    update_timer(s);
    crate::sim::tick(s);
    crate::keypad::tick(s);
    crate::gptimer::tick(s);
    crate::rtc::tick(s);
    crate::audio::tick(uc, s);
    tdma(s);
    if s.dsp_inject.front().is_some_and(|e| s.icount >= e.0) {
        let (_, e) = s.dsp_inject.pop_front().unwrap();
        s.event(format!("inject DSP status {:04x} {:04x} {:04x} {:04x}", e[0], e[1], e[2], e[3]));
        crate::dsp::push_status(uc, s, e);
    }
}

/// Turbo mode: take a pending IRQ if the CPU accepts it (`pc` = next instruction).
pub fn take_irq(uc: &mut Uc<'_>, s: &mut State, pc: u64) -> bool {
    maybe_take_irq(uc, s, pc)
}

pub fn on_block(uc: &mut Uc<'_>, s: &mut State, addr: u64, thumb: bool) {
    // The devices only change state when the OS-timer tick advances (every 40625
    // instructions) — or right after a compare write, until its sync window ends.
    let t = now(s);
    let in_sync = s.soc.timer_armed && s.icount.wrapping_sub(s.soc.compare_written_at) <= TIMER_SYNC_INSNS + 64;
    if t != s.soc.last_tick || in_sync {
        s.soc.last_tick = t;
        update_timer(s);
        crate::sim::tick(s);
        crate::keypad::tick(s);
        crate::gptimer::tick(s);
        crate::rtc::tick(s);
        crate::audio::tick(uc, s);
        tdma(s);
    }
    if s.dsp_inject.front().is_some_and(|e| s.icount >= e.0) {
        let (_, e) = s.dsp_inject.pop_front().unwrap();
        s.event(format!("inject DSP status {:04x} {:04x} {:04x} {:04x}", e[0], e[1], e[2], e[3]));
        crate::dsp::push_status(uc, s, e);
    }
    if s.soc.intc_raw.iter().any(|&r| r != 0) || raw(s, 0) != 0 {
        // maybe_take_irq reads CPSR (I bit) itself
        maybe_take_irq(uc, s, addr | thumb as u64);
    }
}

/// Called when the CPU halted on WFI. Fast-forwards the clock to the next event:
/// the armed timer compare, or else the DSP's next TDMA frame interrupt (the DSP keeps
/// raising it on hardware even when the MCU sleeps). Always returns true: like the
/// real phone, the idle CPU waits for the next interrupt instead of stopping.
pub fn on_wfi(s: &mut State) -> bool {
    if s.turbo.is_some() {
        return wfi_turbo(s);
    }
    s.soc.wfi_count += 1;
    update_timer(s);
    if (0..3).any(|b| raw(s, b) & enable_mask(s, b) != 0) {
        return true; // an enabled interrupt is pending: WFI does not sleep
    }
    // sleep until the next event: timer compare, device deadline or TDMA frame
    let skip = ticks_to_next_event(s);
    if skip > 0 {
        s.soc.clock_skip += skip;
        s.soc.compare_written_at = 0; // time passed while asleep: sync window is over
    }
    update_timer(s);
    crate::sim::tick(s);
    crate::keypad::tick(s);
    crate::gptimer::tick(s);
    crate::rtc::tick(s);
    if tdma(s) {
        s.soc.idle_frames += 1;
    }
    pace(s);
    true
}

/// OS-timer ticks until the next device event (timer compare, device deadlines, the
/// next TDMA frame).
fn ticks_to_next_event(s: &State) -> u64 {
    let n = now(s);
    let mut t = if s.soc.timer_armed { s.soc.timer_compare.wrapping_sub(n) as u64 } else { u64::MAX };
    for d in [crate::sim::next_deadline(s), crate::keypad::next_deadline(s), crate::gptimer::next_deadline(s)].into_iter().flatten() {
        t = t.min(d.wrapping_sub(n) as u64);
    }
    if s.soc.next_frame != 0 {
        t = t.min((s.soc.next_frame.wrapping_sub(n) as i32).max(0) as u64);
    }
    t.min(DSP_FRAME_TICKS)
}

/// The DSP raises its interrupt every GSM TDMA frame (4.615 ms = DSP_FRAME_TICKS),
/// busy or idle. Returns true when a frame interrupt was raised now.
pub fn tdma(s: &mut State) -> bool {
    let n = now(s);
    if s.soc.next_frame == 0 {
        s.soc.next_frame = n.wrapping_add(DSP_FRAME_TICKS as u32).max(1);
        return false;
    }
    if (n.wrapping_sub(s.soc.next_frame) as i32) < 0 {
        return false;
    }
    s.soc.next_frame = n.wrapping_add(DSP_FRAME_TICKS as u32).max(1);
    s.soc.intc_raw[0] |= 1 << DSP_IRQ;
    s.soc.frames += 1;
    true
}

/// Real-time WFI: really sleep until the next event (the clock is the wall clock).
fn wfi_turbo(s: &mut State) -> bool {
    s.soc.wfi_count += 1;
    update_timer(s);
    if (0..3).any(|b| raw(s, b) & enable_mask(s, b) != 0) {
        return true;
    }
    let ticks = ticks_to_next_event(s);
    crate::clock::sleep(std::time::Duration::from_micros(ticks * 1_000_000 / TIMER_HZ));
    sync_clock(s);
    update_timer(s);
    if tdma(s) {
        s.soc.idle_frames += 1;
    }
    true
}

/// Exact mode: the per-tick device work at the current instruction count (as on_block,
/// without taking the IRQ). Returns the instruction count of the next point where a
/// device can change: the next timer tick, or the end of a fresh compare's sync window.
pub fn exact_devices(uc: &mut Uc<'_>, s: &mut State) -> u64 {
    let t = now(s);
    let sync_end = s.soc.compare_written_at + TIMER_SYNC_INSNS;
    let in_sync = s.soc.timer_armed && s.icount <= sync_end + 64;
    if t != s.soc.last_tick || in_sync {
        s.soc.last_tick = t;
        update_timer(s);
        crate::sim::tick(s);
        crate::keypad::tick(s);
        crate::gptimer::tick(s);
        crate::rtc::tick(s);
        crate::audio::tick(uc, s);
        tdma(s);
    }
    if s.dsp_inject.front().is_some_and(|e| s.icount >= e.0) {
        let (_, e) = s.dsp_inject.pop_front().unwrap();
        s.event(format!("inject DSP status {:04x} {:04x} {:04x} {:04x}", e[0], e[1], e[2], e[3]));
        crate::dsp::push_status(uc, s, e);
    }
    let mut next = (s.icount / TIMER_INSNS_PER_TICK + 1) * TIMER_INSNS_PER_TICK;
    if s.soc.timer_armed && s.icount < sync_end {
        next = next.min(sync_end);
    }
    if let Some(&(at, _)) = s.dsp_inject.front() {
        next = next.min(at.max(s.icount + 1));
    }
    next
}
