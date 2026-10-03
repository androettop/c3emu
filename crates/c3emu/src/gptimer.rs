//! General-purpose one-shot timer @0x08830100 (INTC line 11, ISR 0x8095DC1C, driver
//! 0x8095DD00). Used for short hardware delays, e.g. by AUDIO_TA while ramping the
//! audio path (1 ms) — it arms the timer, enables line 11 and blocks on a semaphore
//! that the timer HISR releases. Without the interrupt AUDIO_TA (the audio server,
//! PN 0x62) hangs, the key/startup tones never end and the UI stalls after start-up.
//!
//!   +0x00 status: 0x101 = timer 0 expired (the ISR requires both bits; 0x808 belong to
//!         the second timer, which is the polled delay timer at +0x14/+0x18)
//!   +0x04 control 0: bit 31 = run (0x8B000001 to start, 0x1 / bit 31 clear to stop)
//!   +0x08 load 0 in microseconds (1 MHz; 1000 = 1 ms, 10000 at boot)
//!   +0x0C current count 0
//! The timer 1 registers (+0x14 control with ready bit 0, +0x18) stay on the JSON model.

use crate::machine::State;

pub const GPT_BASE: u64 = 0x0883_0100;
const STATUS: u64 = 0x00;
const CTRL0: u64 = 0x04;
const LOAD0: u64 = 0x08;
const COUNT0: u64 = 0x0C;
pub const GPT_IRQ: u32 = 11;
const RUN: u32 = 1 << 31;
/// Timer 0 expiry sets both bits; the ISR requires status & 0x101 == 0x101.
const EXPIRED0: u32 = 0x101;

#[derive(Default)]
pub struct GpTimerState {
    pub status: u32,
    load: u32,
    /// OS-timer tick at which timer 0 expires (None = stopped).
    deadline: Option<u32>,
    pub expiries: u64,
}

fn us_to_ticks(us: u32) -> u32 {
    ((us as u64 * crate::soc::TIMER_HZ + 999_999) / 1_000_000).max(1) as u32
}

pub fn read(s: &mut State, addr: u64) -> Option<u32> {
    match addr.checked_sub(GPT_BASE)? {
        STATUS => Some(s.gptimer.status),
        COUNT0 => Some(match s.gptimer.deadline {
            // never above the load: the driver waits for "VR loaded" (count <= load)
            Some(d) => ((d.wrapping_sub(crate::soc::now(s)) as u64 * 1_000_000 / crate::soc::TIMER_HZ) as u32)
                .min(s.gptimer.load),
            None => 0,
        }),
        _ => None,
    }
}

pub fn write(s: &mut State, addr: u64, val: u32) {
    match addr.wrapping_sub(GPT_BASE) {
        STATUS => s.gptimer.status &= !val, // write-1-to-clear
        LOAD0 => s.gptimer.load = val,
        CTRL0 => {
            if val & RUN != 0 {
                s.gptimer.status &= !EXPIRED0;
                s.gptimer.deadline = Some(crate::soc::now(s).wrapping_add(us_to_ticks(s.gptimer.load)));
            } else {
                s.gptimer.deadline = None;
            }
        }
        _ => {}
    }
}

pub fn tick(s: &mut State) {
    let Some(d) = s.gptimer.deadline else { return };
    if (crate::soc::now(s).wrapping_sub(d) as i32) >= 0 {
        s.gptimer.deadline = None;
        s.gptimer.status |= EXPIRED0;
        s.gptimer.expiries += 1;
        s.soc.intc_raw[0] |= 1 << GPT_IRQ;
    }
}

pub fn next_deadline(s: &State) -> Option<u32> {
    s.gptimer.deadline
}
