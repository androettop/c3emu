//! Time of day. The OS (src/rtc.c, RTC_GetTime @0x807F6924) keeps the clock as
//! "PMU real-time clock at the last sync + seconds counted since by the watchdog
//! block's counter", and resyncs with the PMU when the two drift apart
//! ("RTC_GetTime: Sync the clock with PMU, rtc_difference = %d").
//!
//! * PMU (BCM59036, i2c1 slave 0x08) RTC registers 0xD4..0xDA: seconds, minutes,
//!   hours, weekday (0 = Sunday), day, month, year - 2000 — the order RTC_GetTime
//!   decodes (masks 0x3F, 0x3F, 0x1F, 0x07, 0x1F, 0x0F, +2000) and in which the
//!   date / time editor writes them (2000-01-01 is weekday 6).
//! * Watchdog block (src/watchdog.c) @0x088A0000, +0x08: free-running seconds.
//!
//! Both run on the phone's clock (emulated time: OS-timer ticks), so they stay
//! deterministic; the time of day starts at `epoch` (seconds since 2000-01-01; the
//! live window and the web page set it to the host's local time).

use crate::machine::State;

pub const RTC_FIRST: u8 = 0xD4;
pub const RTC_LAST: u8 = 0xDA;
pub const WDT_SECONDS: u64 = 0x088A_0008;

#[derive(Default)]
pub struct RtcState {
    /// Register values at `at_tick` (raw, as written).
    fields: Option<[u8; 7]>,
    at_tick: u32,
    /// Time of day at tick 0 when nothing was written yet (seconds since 2000).
    pub epoch: u64,
    /// Minute (since 2000) last seen by tick.
    minute: Option<u64>,
}

fn ticks_to_secs(t: u32) -> u64 {
    t as u64 / crate::soc::TIMER_HZ
}

/// Days since 2000-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 730425 // 1970-01-01 is 719468; 2000-01-01 is 730485 - 60
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 730425;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn to_fields(secs: u64) -> [u8; 7] {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let wday = (days + 6).rem_euclid(7); // 2000-01-01 was a Saturday
    [(rem % 60) as u8, (rem / 60 % 60) as u8, (rem / 3600) as u8, wday as u8, d as u8, m as u8, (y - 2000).clamp(0, 255) as u8]
}

fn from_fields(f: &[u8; 7]) -> u64 {
    let (s, mi, h) = ((f[0] & 0x3F) as u64, (f[1] & 0x3F) as u64, (f[2] & 0x1F) as u64);
    let (d, m, y) = ((f[4] & 0x1F).max(1) as i64, (f[5] & 0x0F).clamp(1, 12) as i64, 2000 + f[6] as i64);
    (days_from_civil(y, m, d).max(0) as u64) * 86400 + h * 3600 + mi * 60 + s
}

/// The register values now.
fn now_fields(s: &State) -> [u8; 7] {
    let now = crate::soc::now(s);
    match s.rtc.fields {
        Some(f) => to_fields(from_fields(&f) + ticks_to_secs(now.wrapping_sub(s.rtc.at_tick))),
        None => to_fields(s.rtc.epoch + ticks_to_secs(now)),
    }
}

/// PMU register read (0xD4..0xDA).
pub fn pmu_read(s: &State, reg: u8) -> u8 {
    now_fields(s)[(reg - RTC_FIRST) as usize]
}

/// PMU register write: the time is set field by field (kept raw until the next read).
pub fn pmu_write(s: &mut State, reg: u8, v: u8) {
    let now = crate::soc::now(s);
    let mut f = if s.rtc.fields.is_some() && s.rtc.at_tick == now { s.rtc.fields.unwrap() } else { now_fields(s) };
    f[(reg - RTC_FIRST) as usize] = v;
    s.rtc.fields = Some(f);
    s.rtc.at_tick = now;
}

/// The watchdog block's seconds counter.
pub fn read(s: &State, addr: u64) -> Option<u32> {
    (addr == WDT_SECONDS).then(|| ticks_to_secs(crate::soc::now(s)) as u32)
}

/// Seconds since 2000-01-01 for a Unix time and UTC offset (host clock, for `epoch`).
pub fn epoch_from_unix(unix: i64, utc_offset_secs: i64) -> u64 {
    (unix + utc_offset_secs - 946_684_800).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar() {
        assert_eq!(to_fields(0), [0, 0, 0, 6, 1, 1, 0]);
        let t = from_fields(&[5, 4, 9, 0, 29, 2, 24]); // 2024-02-29 09:04:05
        assert_eq!(to_fields(t), [5, 4, 9, 4, 29, 2, 24]); // a Thursday
        assert_eq!(to_fields(t + 86400)[4..6], [1, 3]);
    }
}

/// PMU INT1 bit "RTC60S": a minute of the real-time clock passed (enabled by the OS in
/// INT1M; the home screen clock moves on it).
const INT1_RTC60S: u8 = 0x08;
const PMU_INT1M: u8 = 0x0B;

/// Raise the PMU's minute interrupt when the clock enters a new minute.
pub fn tick(s: &mut State) {
    let f = now_fields(s);
    let minute = from_fields(&f) / 60;
    if s.rtc.minute == Some(minute) {
        return;
    }
    let first = s.rtc.minute.is_none();
    s.rtc.minute = Some(minute);
    if first {
        return;
    }
    let key = (1, crate::i2c::PMU, PMU_INT1M);
    if s.i2c.regs.get(&key).copied().unwrap_or(0xFF) & INT1_RTC60S == 0 {
        *s.i2c.regs.entry((1, crate::i2c::PMU, crate::i2c::PMU_INT1)).or_insert(0) |= INT1_RTC60S;
        crate::gpio::raise(s, crate::i2c::PMU_IRQ_GPIO);
    }
}
