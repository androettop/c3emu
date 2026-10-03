//! SIM card interface controller @0x08860000 (simio / hal_sim; IRQ line 4, handler
//! 0x807337A0), modeled with NO card inserted.
//!
//! Registers seen in the activation sequence (driver 0x80853xxx-0x80854xxx):
//!   +0x00 control: power/clock (…28), bit0 set = reset released -> ATR expected
//!   +0x06 u16 status, write-1-to-clear (driver writes 0xFFFF / bit masks)
//!   +0x0E u16 interrupt enable (0x133 while waiting for the ATR)
//!   +0x10, +0x14, +0x1C, +0x20 timing / DMA set-up
//! An absent card never answers the reset: after the ATR window the controller flags
//! "no response" in the status register and interrupts. The interrupt is level-
//! sensitive (status & enable), the OS never acks line 4 at the INTC.

use crate::machine::State;

pub const SIM_BASE: u64 = 0x0886_0000;
const CTRL: u64 = 0x00;
const STATUS: u64 = 0x06;
const IER: u64 = 0x0E;
pub const SIM_IRQ: u32 = 4;
/// ATR window in OS-timer ticks (5120 Hz): ~4 ms.
const ATR_TIMEOUT_TICKS: u32 = 20;

#[derive(Default)]
pub struct SimState {
    pub status: u16,
    pub ier: u16,
    ctrl: u32,
    /// Timer tick at which the "no ATR" event fires (0 = none pending).
    deadline: u32,
    pub timeouts: u64,
}

/// Status bit 1: "no answer to reset" (ISR 0x80733838 moves the driver to state 0xA and
/// reports it; the driver retries at the other voltage class, then gives up and the SIM
/// server reports cardType 0xFF / "SIM is not ready"). Bit 0 is a different event
/// (handled as an error 0xB only when ctrl bit 9 is set).
const STATUS_NO_ATR: u16 = 1 << 1;

pub fn read(s: &mut State, addr: u64) -> Option<u32> {
    match addr.checked_sub(SIM_BASE)? {
        STATUS => Some(s.sim.status as u32),
        _ => None,
    }
}

pub fn write(s: &mut State, addr: u64, val: u32) {
    let Some(off) = addr.checked_sub(SIM_BASE) else { return };
    match off {
        STATUS => s.sim.status &= !(val as u16),
        IER => s.sim.ier = val as u16,
        CTRL => {
            let was = s.sim.ctrl;
            s.sim.ctrl = val;
            if val & 1 == 1 && was & 1 == 0 {
                // reset released: the (absent) card should answer with an ATR
                s.sim.deadline = crate::soc::now(s).wrapping_add(ATR_TIMEOUT_TICKS).max(1);
            } else if val & 1 == 0 {
                s.sim.deadline = 0;
            }
        }
        _ => {}
    }
}

/// Called per block / on idle: fire the pending "no ATR" event.
pub fn tick(s: &mut State) {
    if s.sim.deadline != 0 && (crate::soc::now(s).wrapping_sub(s.sim.deadline) as i32) >= 0 {
        s.sim.deadline = 0;
        s.sim.timeouts += 1;
        s.sim.status |= STATUS_NO_ATR;
        s.event(format!("sim: no ATR (status 0x{:04x}, ier 0x{:04x})", s.sim.status, s.sim.ier));
    }
}

/// The controller's interrupt is level-sensitive (the OS never acks line 4 at the
/// INTC: the ISR clears the cause in +0x06): asserted while status & enable != 0.
pub fn irq_level(s: &State) -> u32 {
    if s.sim.status & s.sim.ier != 0 { 1 << SIM_IRQ } else { 0 }
}

/// Earliest pending deadline (for the idle fast-forward).
pub fn next_deadline(s: &State) -> Option<u32> {
    (s.sim.deadline != 0).then_some(s.sim.deadline)
}
