//! The DSP's audio output, as far as the OS drives it (names from Broadcom's shared.h
//! for the BCM2153 family, which matches this DSP's command/status numbering):
//!
//! * Tone generator (key tones, beeps): COMMAND_PARM_TONE_GENERIC 0x4C (superimpose,
//!   duration ms or 0xFFFF = until stopped, scale), COMMAND_GEN_TONE_GENERIC 0x4B
//!   (freq0, freq1, freq2 in Hz), COMMAND_STOP_TONE 0x06. Key tones are 900/988 Hz.
//! * NEWAUDFIFO (ring tones / start-up tune rendered by the MCU): COMMAND_NEWAUDFIFO_START
//!   0x71 (channel, flags bit0 = stereo, low threshold), _PAUSE 0x72, _RESUME 0x73,
//!   _CANCEL 0x74. 16-bit PCM in a shared-RAM ring at 0x80010000; the control words
//!   shared_NEWAUD_InBuf_in[0] (MCU write index) @0x8000201E, _out[0] (DSP read index)
//!   @0x80002022, _done_flag[0] @0x80002026. The DSP consumes at the sample rate in
//!   STEREOAUDMOD[13:10] (DSP 0xE7D0; 8 = 44.1 kHz) and reports
//!   STATUS_NEWAUDFIFO_SW_FIFO_LOW 0x2A when below the threshold (the MCU refills),
//!   _DONEPLAY 0x2C once the MCU flagged the end and the ring ran dry, _CANCELPLAY 0x2D.
//!
//! Samples are produced in emulated time (OS-timer ticks) and handed to a sink: the
//! live window plays them through the host sound card, `--wav` records them.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::uc::Uc;

use crate::machine::State;

/// Host stream: interleaved stereo i16 at this rate.
pub const OUT_RATE: u32 = 44_100;
const FIFO_BASE: u64 = 0x8001_0000;
/// Ring size in 16-bit words (24 KB, as in Broadcom's ALSA driver; the threshold the OS
/// passes, 0x1800, is half of it).
const FIFO_WORDS: u16 = 0x3000;
const INBUF_IN: u64 = 0x8000_201E;
const INBUF_OUT: u64 = 0x8000_2022;
const DONE_FLAG: u64 = 0x8000_2026;
const STEREOAUDMOD: u16 = 0xE7D0;

const STATUS_FIFO_LOW: u16 = 0x2A;
const STATUS_DONEPLAY: u16 = 0x2C;
const STATUS_CANCELPLAY: u16 = 0x2D;

/// Where produced audio goes.
pub type Sink = Arc<Mutex<VecDeque<i16>>>;

#[derive(Default)]
pub struct AudioState {
    pub sink: Option<Sink>,
    /// Everything produced, if recording (--wav).
    pub record: Option<Vec<i16>>,
    last_tick: Option<u32>,
    /// Fractional output frames carried between ticks (x 5120).
    frac: u64,
    // tone generator
    tone_freqs: [u16; 3],
    tone_scale: u16,
    tone_on: bool,
    tone_end_tick: Option<u32>,
    tone_phase: [f32; 3],
    // NEWAUDFIFO
    fifo_on: bool,
    fifo_paused: bool,
    fifo_stereo: bool,
    fifo_threshold: u16,
    fifo_low_sent: bool,
    fifo_src_pos: f64,
    pub fifo_words_played: u64,
    /// Output frames the FIFO had no data for while playing (the MCU fell behind).
    pub fifo_underruns: u64,
    pub max_in_index: u16,
}

fn rd16(uc: &Uc<'_>, a: u64) -> u16 {
    let mut b = [0u8; 2];
    let _ = uc.mem_read(a, &mut b);
    u16::from_le_bytes(b)
}

fn sample_rate(s: &State) -> u32 {
    let field = (s.dsp_mem.get(&STEREOAUDMOD).copied().unwrap_or(0x2000) >> 10) & 0xF;
    match field {
        0 => 8000,
        1 => 12000,
        2 => 16000,
        3 => 24000,
        4 => 32000,
        5 => 48000,
        6 => 11025,
        7 => 22050,
        _ => 44100,
    }
}

/// A DSP command (called from dsp.rs for every command).
pub fn command(uc: &mut Uc<'_>, s: &mut State, id: u16, a: [u16; 3]) {
    match id {
        0x4C => {
            s.audio.tone_scale = a[2];
            s.audio.tone_end_tick = (a[1] != 0xFFFF).then(|| {
                crate::soc::now(s).wrapping_add((a[1] as u64 * crate::soc::TIMER_HZ / 1000) as u32)
            });
        }
        0x4B => {
            s.audio.tone_freqs = a;
            s.audio.tone_on = true;
        }
        0x05 => {
            // COMMAND_GEN_TONE (tone_id, duration): a plain beep for now
            s.audio.tone_freqs = [1000, 0, 0];
            s.audio.tone_on = true;
            s.audio.tone_end_tick = (a[1] != 0 && a[1] != 0xFFFF)
                .then(|| crate::soc::now(s).wrapping_add((a[1] as u64 * crate::soc::TIMER_HZ / 1000) as u32));
        }
        0x06 => s.audio.tone_on = false,
        0x71 => {
            s.audio.fifo_on = true;
            s.audio.fifo_paused = false;
            s.audio.fifo_stereo = a[1] & 1 != 0;
            s.audio.fifo_threshold = a[2];
            s.audio.fifo_low_sent = false;
            s.audio.fifo_src_pos = 0.0;
            s.event(format!("audio: NEWAUDFIFO start ch{} stereo={} threshold=0x{:x} rate={}",
                            a[0], s.audio.fifo_stereo, a[2], sample_rate(s)));
        }
        0x72 => s.audio.fifo_paused = true,
        0x73 => s.audio.fifo_paused = false,
        0x74 => {
            if s.audio.fifo_on {
                s.audio.fifo_on = false;
                crate::dsp::push_status(uc, s, [STATUS_CANCELPLAY, 1, 0, 0]);
            }
        }
        _ => {}
    }
}

fn emit(s: &mut State, l: i16, r: i16) {
    if let Some(rec) = s.audio.record.as_mut() {
        rec.push(l);
        rec.push(r);
    }
    if let Some(sink) = &s.audio.sink {
        if let Ok(mut q) = sink.lock() {
            if q.len() < OUT_RATE as usize * 2 {
                q.push_back(l);
                q.push_back(r);
            }
        }
    }
}

/// Produce the audio for the emulated time elapsed since the last call.
pub fn tick(uc: &mut Uc<'_>, s: &mut State) {
    let now = crate::soc::now(s);
    let Some(last) = s.audio.last_tick else {
        s.audio.last_tick = Some(now);
        return;
    };
    let dt = now.wrapping_sub(last);
    if dt == 0 {
        return;
    }
    s.audio.last_tick = Some(now);
    if !s.audio.tone_on && !s.audio.fifo_on {
        return;
    }
    // output frames for dt ticks
    let total = s.audio.frac + dt.min(crate::soc::TIMER_HZ as u32) as u64 * OUT_RATE as u64;
    let frames = total / crate::soc::TIMER_HZ;
    s.audio.frac = total % crate::soc::TIMER_HZ;

    if let Some(end) = s.audio.tone_end_tick {
        if (now.wrapping_sub(end) as i32) >= 0 {
            s.audio.tone_on = false;
            s.audio.tone_end_tick = None;
        }
    }
    let rate = sample_rate(s) as f64;
    let step = rate / OUT_RATE as f64;
    for _ in 0..frames {
        let mut l = 0f32;
        let mut r = 0f32;
        if s.audio.tone_on {
            let mut v = 0f32;
            let n = s.audio.tone_freqs.iter().filter(|&&f| f != 0).count().max(1) as f32;
            for i in 0..3 {
                let f = s.audio.tone_freqs[i];
                if f == 0 {
                    continue;
                }
                s.audio.tone_phase[i] = (s.audio.tone_phase[i] + f as f32 / OUT_RATE as f32).fract();
                v += (s.audio.tone_phase[i] * std::f32::consts::TAU).sin() / n;
            }
            l += v * 0.25;
            r += v * 0.25;
        }
        if s.audio.fifo_on && !s.audio.fifo_paused {
            let inp = rd16(uc, INBUF_IN) % FIFO_WORDS;
            s.audio.max_in_index = s.audio.max_in_index.max(inp);
            let mut out = rd16(uc, INBUF_OUT) % FIFO_WORDS;
            let ch = if s.audio.fifo_stereo { 2 } else { 1 };
            s.audio.fifo_src_pos += step;
            while s.audio.fifo_src_pos >= 1.0 {
                s.audio.fifo_src_pos -= 1.0;
                let avail = (inp + FIFO_WORDS - out) % FIFO_WORDS;
                if avail < ch {
                    if rd16(uc, DONE_FLAG) == 0 {
                        s.audio.fifo_underruns += 1;
                    }
                    break;
                }
                let a0 = rd16(uc, FIFO_BASE + 2 * out as u64) as i16;
                let a1 = if ch == 2 { rd16(uc, FIFO_BASE + 2 * ((out + 1) % FIFO_WORDS) as u64) as i16 } else { a0 };
                out = (out + ch) % FIFO_WORDS;
                s.audio.fifo_words_played += ch as u64;
                l += a0 as f32 / 32768.0;
                r += a1 as f32 / 32768.0;
            }
            let _ = uc.mem_write(INBUF_OUT, &out.to_le_bytes());
        }
        emit(s, (l.clamp(-1.0, 1.0) * 32767.0) as i16, (r.clamp(-1.0, 1.0) * 32767.0) as i16);
    }
    if s.audio.fifo_on && !s.audio.fifo_paused {
        let inp = rd16(uc, INBUF_IN) % FIFO_WORDS;
        let out = rd16(uc, INBUF_OUT) % FIFO_WORDS;
        let avail = (inp + FIFO_WORDS - out) % FIFO_WORDS;
        if rd16(uc, DONE_FLAG) != 0 && avail < 2 {
            s.audio.fifo_on = false;
            s.event("audio: NEWAUDFIFO done".into());
            crate::dsp::push_status(uc, s, [STATUS_DONEPLAY, 1, 0, 0]);
        } else if avail < s.audio.fifo_threshold {
            if !s.audio.fifo_low_sent {
                s.audio.fifo_low_sent = true;
                s.event(format!("audio: FIFO low in=0x{inp:x} out=0x{out:x}"));
                crate::dsp::push_status(uc, s, [STATUS_FIFO_LOW, 1, 0, 0]);
            }
        } else {
            s.audio.fifo_low_sent = false;
        }
    }
}

/// Write a recording as a 16-bit stereo WAV file.
pub fn write_wav(path: &str, samples: &[i16]) -> std::io::Result<()> {
    use std::io::Write;
    let data_len = (samples.len() * 2) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&2u16.to_le_bytes())?; // stereo
    f.write_all(&OUT_RATE.to_le_bytes())?;
    f.write_all(&(OUT_RATE * 4).to_le_bytes())?;
    f.write_all(&4u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}
