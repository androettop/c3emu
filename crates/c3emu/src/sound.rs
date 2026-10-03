//! Host sound output for the live window: plays the phone's audio (audio.rs sink,
//! interleaved stereo i16 at audio::OUT_RATE) through the default output device.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio::{Sink, OUT_RATE};

/// Open the default output device and start pulling from `sink`. The returned stream
/// must be kept alive. None if there is no usable device (the emulator runs silent).
pub fn start(sink: Sink) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let dev = host.default_output_device()?;
    let def = dev.default_output_config().ok()?;
    let channels = def.channels().max(1) as usize;
    let dev_rate = def.sample_rate().0;
    // try the phone's rate first, else the device's own rate with a simple resampler
    let attempts = [OUT_RATE, dev_rate];
    for rate in attempts {
        let cfg = cpal::StreamConfig {
            channels: channels as u16,
            sample_rate: cpal::SampleRate(rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let q = sink.clone();
        let step = OUT_RATE as f64 / rate as f64;
        let mut pos = 0f64;
        let mut cur = (0i16, 0i16);
        let stream = dev.build_output_stream(
            &cfg,
            move |out: &mut [f32], _| {
                let mut q = match q.lock() {
                    Ok(q) => q,
                    Err(_) => return,
                };
                for frame in out.chunks_mut(channels) {
                    pos += step;
                    while pos >= 1.0 {
                        pos -= 1.0;
                        if q.len() >= 2 {
                            cur = (q.pop_front().unwrap(), q.pop_front().unwrap());
                        } else {
                            cur = (0, 0); // underrun: silence
                        }
                    }
                    let (l, r) = (cur.0 as f32 / 32768.0, cur.1 as f32 / 32768.0);
                    for (c, s) in frame.iter_mut().enumerate() {
                        *s = match c {
                            0 => l,
                            1 => r,
                            _ => (l + r) / 2.0,
                        };
                    }
                }
            },
            |e| eprintln!("sound: {e}"),
            None,
        );
        if let Ok(s) = stream {
            if s.play().is_ok() {
                return Some(s);
            }
        }
    }
    None
}
