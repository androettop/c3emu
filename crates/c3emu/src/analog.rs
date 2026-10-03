//! Analog front-end modeled at the ADC driver API (src/dacmgr.c / adc_chipset_api.c).
//!
//! adc_read(channel, callback) @0x807008F0 posts a request to the ADC task and blocks on
//! its reply queue; it returns the 10-bit reading or an error (0xFFFD busy, 0xFFFE
//! failed, 0xFFFF). Without the real converter timing the request never completes and
//! callers (e.g. the EM init in MTC_CTRL via 0x809BCC40) retry forever. Synchronous
//! calls (callback == 0) are answered here with a nominal reading per channel; calls
//! with a callback still go to the real driver.

use crate::uc::{RegisterARM, Uc};

use crate::machine::{Machine, State};

const ADC_READ: u64 = 0x8070_08F0;

/// Channel 6 is the battery voltage: ADCUTL_Adc2VBatt (0x80700CDC) converts it with
/// 4687 uV/bit and offset 0 ([0x80F65778 + 0x68/0x6C]); boot needs >= 0x240 (2.7 V,
/// check @0x809BCC7A).
pub const VBAT_CHANNEL: u32 = 6;
const UV_PER_BIT: u32 = 4687;
/// Battery voltage: 4.0 V = BATTMGR level 5 (full scale; low threshold 0x2D9 = 3.42 V).
pub const VBAT_MV: u32 = 4000;

/// Nominal 10-bit reading per channel.
fn reading(channel: u32) -> u32 {
    match channel {
        VBAT_CHANNEL => (VBAT_MV * 1000 + UV_PER_BIT / 2) / UV_PER_BIT,
        _ => 0x200,
    }
}

pub fn install(m: &mut Machine) {
    m.hle_fn(ADC_READ, "adc_read", Box::new(|uc: &mut Uc<'_>, s: &mut State| {
        let (ch, cb) = (uc.reg_read(RegisterARM::R0).unwrap_or(0) as u32,
                        uc.reg_read(RegisterARM::R1).unwrap_or(0));
        if cb != 0 {
            // can't run the guest callback from here: let the real driver handle it
            *s.hle_calls.entry("adc_read (driver, async)".into()).or_default() += 1;
            return None;
        }
        *s.hle_calls.entry(format!("adc_read ch{ch}")).or_default() += 1;
        Some(reading(ch))
    }));
}
