//! Resampling to 16 kHz mono i16 and the crash-tolerant WAV spool.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const RATE: u32 = 16_000;
const HEADER_LEN: u32 = 44;

/// Streaming resampler from the device rate to 16 kHz. State carries across chunks.
pub enum Resampler {
    /// Exactly 48 kHz: average each group of 3 input samples.
    Third { acc: f32, n: u8 },
    /// Any other rate: linear interpolation, delayed by one input sample so a chunk never
    /// needs the next one. `pos` is the next output position in 1/16000ths of an input
    /// sample (exact integer maths, so chunking can't change the output); `prev` is the
    /// last sample of the previous chunk, at index -1.
    Linear { in_rate: u64, pos: u64, prev: f32 },
}

impl Resampler {
    pub fn new(in_rate: u32) -> Resampler {
        if in_rate == 48_000 {
            Resampler::Third { acc: 0.0, n: 0 }
        } else {
            Resampler::Linear {
                in_rate: u64::from(in_rate),
                pos: u64::from(RATE),
                prev: 0.0,
            }
        }
    }

    /// Appends resampled i16 samples for `input` to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<i16>) {
        match self {
            Resampler::Third { acc, n } => {
                for &s in input {
                    *acc += s;
                    *n += 1;
                    if *n == 3 {
                        out.push(to_i16(*acc / 3.0));
                        *acc = 0.0;
                        *n = 0;
                    }
                }
            }
            Resampler::Linear { in_rate, pos, prev } => {
                let rate = u64::from(RATE);
                let end = input.len() as u64 * rate;
                // Sample at index i, where -1 means `prev`.
                let at = |i: i64| if i < 0 { *prev } else { input[i as usize] };
                while *pos < end {
                    let i = (*pos / rate) as i64;
                    let frac = (*pos % rate) as f32 / RATE as f32;
                    let (a, b) = (at(i - 1), at(i));
                    out.push(to_i16(a + (b - a) * frac));
                    *pos += *in_rate;
                }
                if let Some(&last) = input.last() {
                    *prev = last;
                    *pos -= end;
                }
            }
        }
    }
}

pub fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

pub fn header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let fields: [(usize, &[u8]); 13] = [
        (0, b"RIFF"),
        (4, &(36 + data_len).to_le_bytes()),
        (8, b"WAVE"),
        (12, b"fmt "),
        (16, &16u32.to_le_bytes()),
        (20, &1u16.to_le_bytes()), // PCM
        (22, &1u16.to_le_bytes()), // mono
        (24, &RATE.to_le_bytes()),
        (28, &(RATE * 2).to_le_bytes()),
        (32, &2u16.to_le_bytes()),
        (34, &16u16.to_le_bytes()),
        (36, b"data"),
        (40, &data_len.to_le_bytes()),
    ];
    for (at, bytes) in fields {
        h[at..at + bytes.len()].copy_from_slice(bytes);
    }
    h
}

/// 16 kHz mono i16 WAV written incrementally. Sizes are patched every ~1 s and on finish,
/// so a crash leaves a playable file; `repair` fixes it exactly from the file length.
pub struct WavSpool {
    file: File,
    data_len: u32,
    unpatched: u32,
}

impl WavSpool {
    pub fn create(path: &Path) -> io::Result<WavSpool> {
        let mut file = File::create(path)?;
        file.write_all(&header(0))?;
        Ok(WavSpool {
            file,
            data_len: 0,
            unpatched: 0,
        })
    }

    pub fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        self.file.write_all(&bytes)?;
        self.data_len += bytes.len() as u32;
        self.unpatched += bytes.len() as u32;
        if self.unpatched >= RATE * 2 {
            self.patch()?;
        }
        Ok(())
    }

    fn patch(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header(self.data_len))?;
        self.file.seek(SeekFrom::End(0))?;
        self.unpatched = 0;
        Ok(())
    }

    /// Final header patch and flush to disk. Returns the audio duration in ms.
    pub fn finish(mut self) -> io::Result<u64> {
        self.patch()?;
        self.file.sync_all()?;
        Ok(duration_ms(self.data_len))
    }
}

pub fn duration_ms(data_len: u32) -> u64 {
    u64::from(data_len / 2) * 1000 / u64::from(RATE)
}

/// Rewrites the header of a spool file left by a crash. Returns its duration in ms.
pub fn repair(path: &Path) -> io::Result<u64> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    if len < u64::from(HEADER_LEN) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "spool file too short",
        ));
    }
    let data_len = ((len - u64::from(HEADER_LEN)).min(u64::from(u32::MAX - 36)) & !1) as u32;
    file.write_all(&header(data_len))?;
    Ok(duration_ms(data_len))
}

/// Reads a WAV file written by `WavSpool` (for batch retry).
pub fn read_wav(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * 0.5)
            .collect()
    }

    fn resample_chunked(rate: u32, input: &[f32], chunk: usize) -> Vec<i16> {
        let mut r = Resampler::new(rate);
        let mut out = Vec::new();
        for c in input.chunks(chunk) {
            r.process(c, &mut out);
        }
        out
    }

    fn zero_crossings(s: &[i16]) -> usize {
        s.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count()
    }

    #[test]
    fn lengths_and_chunking_do_not_matter() {
        for rate in [48_000, 44_100, 32_000, 16_000, 8_000] {
            let input = sine(rate, 1000.0, 1.0);
            let whole = resample_chunked(rate, &input, input.len());
            // The one-sample delay drops at most 16000/rate samples at the end.
            assert!(
                (whole.len() as i64 - 16_000).abs() <= 2,
                "{rate}: {}",
                whole.len()
            );
            for chunk in [1, 7, 480, 441] {
                assert_eq!(
                    resample_chunked(rate, &input, chunk),
                    whole,
                    "{rate}/{chunk}"
                );
            }
        }
    }

    #[test]
    fn frequency_is_preserved() {
        for rate in [48_000, 44_100] {
            let out = resample_chunked(rate, &sine(rate, 1000.0, 1.0), 512);
            let zc = zero_crossings(&out);
            assert!((1990..=2010).contains(&zc), "{rate}: {zc}");
        }
    }

    #[test]
    fn spool_header_and_repair() {
        let dir = std::env::temp_dir().join(format!("gemdict-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wav");
        let mut w = WavSpool::create(&path).unwrap();
        w.write(&vec![100i16; 16_000]).unwrap(); // 1 s: triggers a patch
        w.write(&vec![100i16; 8_000]).unwrap(); // 0.5 s more: no patch yet
        let bytes = read_wav(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            32_000
        );
        assert_eq!(w.finish().unwrap(), 1500);
        let bytes = read_wav(&path).unwrap();
        assert_eq!(bytes.len(), 44 + 48_000);
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            48_000
        );
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            36 + 48_000
        );

        // Simulate a crash: stale header, extra data appended.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[0u8; 3201]).unwrap();
        drop(f);
        assert_eq!(repair(&path).unwrap(), 1600);
        let bytes = read_wav(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            51_200
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
