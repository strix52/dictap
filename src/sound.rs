//! Short start/stop cues, synthesized once into in-memory WAVs.

use std::sync::OnceLock;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::core::PCWSTR;

/// A soft sine blip; `rising` sweeps up for start, down for stop.
fn blip(rising: bool) -> Vec<u8> {
    const RATE: f32 = 16_000.0;
    let n = 1_900; // ~120 ms
    let (f0, f1) = if rising { (660.0, 990.0) } else { (990.0, 660.0) };
    let mut phase = 0f32;
    let samples: Vec<i16> = (0..n)
        .map(|i| {
            let t = i as f32 / n as f32;
            phase += std::f32::consts::TAU * (f0 + (f1 - f0) * t) / RATE;
            let env = (t * 20.0).min(1.0) * (1.0 - t).powi(2);
            (phase.sin() * env * 6_000.0) as i16
        })
        .collect();
    let mut wav = crate::audio::header(samples.len() as u32 * 2).to_vec();
    wav.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
    wav
}

fn play(buf: &'static [u8]) {
    // SAFETY: SND_MEMORY reads the buffer asynchronously; it is 'static.
    unsafe {
        let _ = PlaySoundW(
            PCWSTR(buf.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

pub fn start() {
    static BUF: OnceLock<Vec<u8>> = OnceLock::new();
    play(BUF.get_or_init(|| blip(true)));
}

pub fn stop() {
    static BUF: OnceLock<Vec<u8>> = OnceLock::new();
    play(BUF.get_or_init(|| blip(false)));
}
