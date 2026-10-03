//! Broadcom BSC (I2C) controllers, as driven by src/i2c.c + src/i2c_chipset_api.c.
//!
//! Two controllers: I2C1 @0x088A0020 (IRQ 7), I2C2 @0x088B0020 (IRQ 13). Registers
//! (offsets from that base, 8-bit), derived from the driver:
//!   +0x00 CS    bit0 enable; bits[3:1] command: START (0xC3), STOP (0xC5), RESTART
//!               (|0x0A), read with ACK/NACK (|0x06 / |0x0E)  (0x8077CA58..0x8077CB06)
//!   +0x04 TIM, +0x0C TOUT, +0x10/+0x14 CRC, +0x1C TX FIFO control, +0x2C clock enable
//!   +0x08 DAT   write = send a byte, read = received byte
//!   +0x24 IER   interrupt enable (driver writes 0x8D)
//!   +0x28 ISR   write-1-to-clear. bit7 busy, bit3 session done (success), bits 0/2
//!               error/no-ack: wait @0x8077CB74 ends on ISR & 0x0D, success iff
//!               ISR & 0x05 == 0 (0x8077CC10).
//! Model: every command / data byte completes at once with "session done" and the
//! controller's interrupt; every slave acks; reads return I2C_READ.
//! The bus log records which slave addresses the OS talks to.

use std::collections::BTreeMap;

use crate::machine::State;

pub const BUSES: &[(u64, u32)] = &[(0x088A_0020, 7), (0x088B_0020, 13)];
const CS: u64 = 0x00;
const DAT: u64 = 0x08;
const IER: u64 = 0x24;
const ISR: u64 = 0x28;
const ISR_DONE: u8 = 0x08;
/// Value returned for every byte read from a slave.
const I2C_READ: u8 = 0x00;

#[derive(Default)]
pub struct I2cState {
    pub isr: [u8; 2],
    pub ier: [u8; 2],
    /// Next DAT write after a START/RESTART is the slave address byte.
    expect_addr: [bool; 2],
    /// Current slave (7-bit) and whether the next written byte is the register index.
    slave: [u8; 2],
    expect_reg: [bool; 2],
    /// Last register index written per bus (reads continue from it).
    reg: [u8; 2],
    /// (bus, slave, register, 'R'/'W') -> count
    pub accesses: BTreeMap<(usize, u8, u8, char), u64>,
    /// Register files of the slaves: (bus, slave, register) -> value (written values
    /// read back; see `fixed` for registers with modeled behaviour).
    pub regs: BTreeMap<(usize, u8, u8), u8>,
    pub slaves: BTreeMap<(usize, u8), u64>,
}

/// PMU (Broadcom BCM59036, i2c1 slave 0x08). From the OS's PMU driver and its log:
/// "PMU_Read: read PMU_REG_ENV1~4" reads from 0x4C, "PMU_REG_INT1~10" from 0x01.
/// PMU_DRV_GetPowerupCause_Cb (0x807D8FF8): INT1 bit0 -> "power up through power on
/// key"; ENV2 (0x4D) bit0 -> "power up key is down". Interrupt registers clear on read.
pub const PMU: u8 = 0x08;
pub const PMU_INT1: u8 = 0x01;
pub const PMU_INT10: u8 = 0x0A;

fn is_clear_on_read(key: (usize, u8, u8)) -> bool {
    key.0 == 1 && key.1 == PMU && (PMU_INT1..=PMU_INT10).contains(&key.2)
}

/// Power-on state of the slaves' registers: the phone was switched on with the power
/// key (pending INT1 bit0).
pub fn power_on(s: &mut I2cState) {
    s.regs.insert((1, PMU, PMU_INT1), 0x01);
}

/// GPIO pin of the PMU interrupt ("PMU_DRV_IntInitCb: ... IntLine = 21").
pub const PMU_IRQ_GPIO: u32 = 21;
const PMU_ENV2: u8 = 0x4D;
/// INT1 power-on-key bits (Linux bcm59035.h: PONKEYBR 0x01 release, PONKEYBF 0x02 press,
/// PONKEYBH 0x04 hold).
const INT1_PONKEY_RELEASE: u8 = 0x01;
const INT1_PONKEY_PRESS: u8 = 0x02;

/// The power / end-call key (red key): wired to the PMU's PONKEY input, not to the
/// keypad matrix. Latches the press/release interrupt, updates ENV2 bit0 (key down)
/// and raises the PMU interrupt (GPIO 21).
pub fn power_key(s: &mut State, down: bool) {
    let int1 = s.i2c.regs.entry((1, PMU, PMU_INT1)).or_insert(0);
    *int1 |= if down { INT1_PONKEY_PRESS } else { INT1_PONKEY_RELEASE };
    let env2 = s.i2c.regs.entry((1, PMU, PMU_ENV2)).or_insert(0);
    if down { *env2 |= 1 } else { *env2 &= !1 }
    crate::gpio::raise(s, PMU_IRQ_GPIO);
    s.event(format!("power key {}", if down { "down" } else { "up" }));
}

/// Registers with modeled values: (bus, slave, register) -> value.
fn fixed(key: (usize, u8, u8)) -> Option<u8> {
    match key {
        _ => None,
    }
}

fn bus_of(addr: u64) -> Option<(usize, u64, u32)> {
    BUSES.iter().enumerate()
        .find(|(_, (base, _))| (*base..*base + 0x30).contains(&addr))
        .map(|(i, &(base, irq))| (i, addr - base, irq))
}

/// Bus read: ISR and received data.
pub fn read(s: &mut State, addr: u64) -> Option<u32> {
    let (bus, off, _) = bus_of(addr)?;
    match off {
        ISR => Some(s.i2c.isr[bus] as u32),
        DAT => {
            let key = (bus + 1, s.i2c.slave[bus], s.i2c.reg[bus]);
            *s.i2c.accesses.entry((key.0, key.1, key.2, 'R')).or_default() += 1;
            let v = if key.0 == 1 && key.1 == PMU && (crate::rtc::RTC_FIRST..=crate::rtc::RTC_LAST).contains(&key.2) {
                crate::rtc::pmu_read(s, key.2)
            } else {
                fixed(key).or_else(|| s.i2c.regs.get(&key).copied()).unwrap_or(I2C_READ)
            };
            if is_clear_on_read(key) {
                s.i2c.regs.insert(key, 0);
            }
            s.i2c.reg[bus] = s.i2c.reg[bus].wrapping_add(1); // sequential read
            Some(v as u32)
        }
        _ => None,
    }
}

/// Bus write: commands, data bytes, ISR clear, IER.
pub fn write(s: &mut State, addr: u64, val: u32) {
    let Some((bus, off, irq)) = bus_of(addr) else { return };
    let v = val as u8;
    let complete = match off {
        ISR => {
            s.i2c.isr[bus] &= !v;
            false
        }
        IER => {
            s.i2c.ier[bus] = v;
            false
        }
        CS => {
            let cmd = (v >> 1) & 7;
            if cmd == 1 || cmd == 5 {
                s.i2c.expect_addr[bus] = true; // START / RESTART
            }
            cmd != 0
        }
        DAT => {
            if s.i2c.expect_addr[bus] {
                s.i2c.expect_addr[bus] = false;
                s.i2c.slave[bus] = v >> 1;
                s.i2c.expect_reg[bus] = v & 1 == 0; // write transfer: register index next
                if v & 1 == 1 {
                    let (sl, r) = (v >> 1, s.i2c.reg[bus]);
                    s.event(format!("i2c{} read slave 0x{sl:02x} from reg 0x{r:02x}", bus + 1));
                }
                *s.i2c.slaves.entry((bus + 1, v >> 1)).or_default() += 1;
            } else if s.i2c.expect_reg[bus] {
                s.i2c.expect_reg[bus] = false;
                s.i2c.reg[bus] = v;
            } else {
                let key = (bus + 1, s.i2c.slave[bus], s.i2c.reg[bus]);
                *s.i2c.accesses.entry((key.0, key.1, key.2, 'W')).or_default() += 1;
                s.i2c.regs.insert(key, v);
                if key.0 == 1 && key.1 == PMU && (crate::rtc::RTC_FIRST..=crate::rtc::RTC_LAST).contains(&key.2) {
                    crate::rtc::pmu_write(s, key.2, v);
                }
                s.i2c.reg[bus] = s.i2c.reg[bus].wrapping_add(1);
            }
            true
        }
        _ => false,
    };
    if complete {
        s.i2c.isr[bus] |= ISR_DONE;
        if s.i2c.ier[bus] & ISR_DONE != 0 {
            s.soc.intc_raw[(irq / 32) as usize] |= 1 << (irq % 32);
        }
    }
}
