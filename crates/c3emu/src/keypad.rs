//! Keypad matrix scanner @0x088CE080 (inside the GPIO block; CAL_keypad.c, init
//! 0x80723A50 with 8 rows x 7 columns, LISR 0x807C1C6C on INTC line 5, HISR 0x80723BCC).
//!
//!   +0x00 control, +0x04 I/O config, +0x10..+0x1C interrupt clear/enable (init 0xFFFFFFFF)
//!   +0x20 / +0x24  key state, active low: bit n = key (row n>>3, column n&7), keys 0-31
//!                  in +0x20, 32-63 in +0x24 (HISR reads both, inverts them and diffs
//!                  against the previous scan, then calls the CAL callback per change)
//!   +0x30 / +0x34  edge configuration
//! A key change latches line 5 (the LISR masks and acks it, the HISR re-enables it).

use std::collections::VecDeque;

use crate::machine::State;

pub const KPD_BASE: u64 = 0x088C_E080;
const STATE_LO: u64 = 0x20;
const STATE_HI: u64 = 0x24;
pub const KPD_IRQ: u32 = 5;
/// How long an injected key stays down, in OS-timer ticks (5120 Hz): 120 ms.
pub const HOLD_TICKS: u32 = 614;

#[derive(Default)]
pub struct KeypadState {
    /// Bit n set = key n held.
    pub pressed: u64,
    /// Scheduled presses: (icount, key, hold in OS ticks).
    pub script: VecDeque<(u64, u8, u32)>,
    /// Pending releases: (timer tick, key).
    releases: Vec<(u32, u8)>,
    pub changes: u64,
    /// Host keys held: key -> (tick pressed, keypad scans by the OS at that time).
    host_down: crate::fxhash::FxHashMap<u8, (u32, u64)>,
    /// Host taps waiting to be released: (earliest tick, scans at press, key).
    taps: Vec<(u32, u64, u8)>,
    /// Reads of the key state by the OS (its scans).
    scans: u64,
}

pub fn read(s: &mut State, addr: u64) -> Option<u32> {
    match addr.checked_sub(KPD_BASE)? {
        STATE_LO => {
            s.keypad.scans += 1;
            Some(!(s.keypad.pressed as u32))
        }
        STATE_HI => Some(!((s.keypad.pressed >> 32) as u32)),
        _ => None,
    }
}

/// Pseudo key index for the red (power / end) key, which sits on the PMU.
pub const POWER_KEY: u8 = 255;

/// Press or release key `key` (row * 8 + column, or POWER_KEY).
pub fn set_key(s: &mut State, key: u8, down: bool) {
    if key == POWER_KEY {
        return crate::i2c::power_key(s, down);
    }
    let bit = 1u64 << (key & 63);
    let was = s.keypad.pressed;
    if down {
        s.keypad.pressed |= bit;
    } else {
        s.keypad.pressed &= !bit;
    }
    if s.keypad.pressed != was {
        s.keypad.changes += 1;
        s.soc.intc_raw[0] |= 1 << KPD_IRQ;
        s.event(format!("key {key} {}", if down { "down" } else { "up" }));
    }
}

/// True while `key` is down because of a tap (released by the timer, not the host).
pub fn tapping(s: &State, key: u8) -> bool {
    s.keypad.releases.iter().any(|r| r.1 == key)
}

/// Tap a key now (released HOLD_TICKS later).
pub fn tap(s: &mut State, key: u8) {
    hold(s, key, HOLD_TICKS);
}

/// Press a key now and release it `ticks` OS-timer ticks later.
pub fn hold(s: &mut State, key: u8, ticks: u32) {
    set_key(s, key, true);
    let t = crate::soc::now(s).wrapping_add(ticks.max(1));
    s.keypad.releases.push((t, key));
}

/// Release a held key `ticks` OS-timer ticks from now (a host key released too soon for
/// the OS to have seen it).
pub fn release_after(s: &mut State, key: u8, ticks: u32) {
    let t = crate::soc::now(s).wrapping_add(ticks.max(1));
    s.keypad.releases.push((t, key));
}

/// A host key shorter than this is a single press (tap); longer, a long press.
pub const TAP_MS: u64 = 300;
/// Shortest tap the OS gets to see.
const TAP_MIN_MS: u64 = 60;

fn ms_ticks(ms: u64) -> u32 {
    (ms * crate::soc::TIMER_HZ / 1000) as u32
}

/// A key of the host (PC keyboard, mouse) went down. Auto-repeat is ignored.
pub fn host_down(s: &mut State, key: u8) {
    if s.keypad.host_down.contains_key(&key) {
        return;
    }
    if s.keypad.taps.iter().any(|t| t.2 == key) {
        s.keypad.taps.retain(|t| t.2 != key);
        set_key(s, key, false); // the previous tap of this key ends now
    }
    let at = (crate::soc::now(s), s.keypad.scans);
    s.keypad.host_down.insert(key, at);
    set_key(s, key, true);
}

/// A key of the host went up. Held less than TAP_MS: a single press, released once the
/// OS has scanned it (and at least TAP_MIN_MS after the press) — so a short press is
/// never seen as held, or twice, even when the emulator lags. Otherwise a long press,
/// released now.
pub fn host_up(s: &mut State, key: u8) {
    let Some((t, scans)) = s.keypad.host_down.remove(&key) else { return };
    if crate::soc::now(s).wrapping_sub(t) < ms_ticks(TAP_MS) {
        s.keypad.taps.push((t.wrapping_add(ms_ticks(TAP_MIN_MS)), scans, key));
    } else {
        set_key(s, key, false);
    }
}

/// Keys the host holds now.
pub fn host_held(s: &State) -> Vec<u8> {
    s.keypad.host_down.keys().copied().collect()
}

pub fn tick(s: &mut State) {
    if !s.keypad.taps.is_empty() {
        let n = crate::soc::now(s);
        let scans = s.keypad.scans;
        let due: Vec<u8> = s.keypad.taps.iter()
            .filter(|t| (n.wrapping_sub(t.0) as i32) >= 0 && (scans > t.1 || t.2 == POWER_KEY))
            .map(|t| t.2).collect();
        if !due.is_empty() {
            s.keypad.taps.retain(|t| !due.contains(&t.2));
            for k in due {
                set_key(s, k, false);
            }
        }
    }
    while s.keypad.script.front().is_some_and(|&(at, _, _)| s.icount >= at) {
        let (_, key, ticks) = s.keypad.script.pop_front().unwrap();
        hold(s, key, ticks);
    }
    if s.keypad.releases.is_empty() {
        return;
    }
    let n = crate::soc::now(s);
    let due: Vec<u8> = s.keypad.releases.iter().filter(|r| (n.wrapping_sub(r.0) as i32) >= 0).map(|r| r.1).collect();
    if !due.is_empty() {
        s.keypad.releases.retain(|r| (n.wrapping_sub(r.0) as i32) < 0);
        for k in due {
            set_key(s, k, false);
        }
    }
}

/// Earliest pending release (for the idle fast-forward).
pub fn next_deadline(s: &State) -> Option<u32> {
    let n = crate::soc::now(s);
    s.keypad.releases.iter().map(|r| r.0).chain(s.keypad.taps.iter().map(|t| t.0)).min_by_key(|&t| t.wrapping_sub(n))
}
