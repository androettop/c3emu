//! GPIO interrupts @0x088CE000 (INTC line 6, ISR 0x8075C494): the ISR loops while
//! (status & mask) != 0 over +0x50 / +0x54 (pins 0-31 / 32-63) with masks +0x30 / +0x34,
//! calling the per-pin callbacks, which acknowledge by writing the pin bit back
//! (write-1-to-clear). The PMU interrupt output is GPIO 21 ("PMU_DRV_IntInitCb: Enable
//! PMU Interrupt; IntLine = 21").

use crate::machine::State;

const STATUS_LO: u64 = 0x088C_E050;
const STATUS_HI: u64 = 0x088C_E054;
pub const GPIO_IRQ: u32 = 6;

#[derive(Default)]
pub struct GpioState {
    pub status: [u32; 2],
}

/// Latch an interrupt on `pin` and raise the GPIO line.
pub fn raise(s: &mut State, pin: u32) {
    s.gpio.status[(pin / 32) as usize & 1] |= 1 << (pin % 32);
    s.soc.intc_raw[0] |= 1 << GPIO_IRQ;
}

pub fn read(s: &mut State, addr: u64) -> Option<u32> {
    match addr {
        STATUS_LO => Some(s.gpio.status[0]),
        STATUS_HI => Some(s.gpio.status[1]),
        _ => None,
    }
}

pub fn write(s: &mut State, addr: u64, val: u32) {
    match addr {
        STATUS_LO => s.gpio.status[0] &= !val,
        STATUS_HI => s.gpio.status[1] &= !val,
        _ => {}
    }
}
