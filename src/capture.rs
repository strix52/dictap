//! Per-dictation microphone capture. The cpal callback only downmixes and hands chunks
//! over; this thread resamples, spools the WAV and feeds the Live queue. It never touches
//! the network.

use crate::audio::{RATE, Resampler, WavSpool};
use crate::event::{CaptureEvent, Event};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Live queue cap: 15 s of 16 kHz audio. Past this, Live is abandoned and batch uses the WAV.
const QUEUE_CAP: usize = 15 * RATE as usize;

/// At most one capture thread alive; a stuck open keeps this set until it returns.
static BUSY: AtomicBool = AtomicBool::new(false);

pub fn busy() -> bool {
    BUSY.load(Ordering::Acquire)
}

pub enum Pop {
    Data(Vec<i16>),
    /// Nothing arrived within the timeout.
    Empty,
    /// Capture ended and everything was taken.
    Closed,
    /// Live fell more than `QUEUE_CAP` behind.
    Overflowed,
}

#[derive(Default)]
struct QueueState {
    samples: VecDeque<i16>,
    closed: bool,
    overflowed: bool,
}

/// 16 kHz samples from capture to the Live sender.
#[derive(Default)]
pub struct LiveQueue {
    state: Mutex<QueueState>,
    cv: Condvar,
}

impl LiveQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push(&self, samples: &[i16]) {
        let mut s = self.lock();
        if s.overflowed {
            return;
        }
        if s.samples.len() + samples.len() > QUEUE_CAP {
            s.overflowed = true;
            s.samples = VecDeque::new();
        } else {
            s.samples.extend(samples);
        }
        self.cv.notify_one();
    }

    pub fn close(&self) {
        self.lock().closed = true;
        self.cv.notify_one();
    }

    /// Takes exactly `frame` samples once that many are queued, waiting up to `timeout`.
    /// After capture closes, a shorter remainder is returned.
    pub fn pop(&self, frame: usize, timeout: Duration) -> Pop {
        let s = self.lock();
        let (mut s, _) = self
            .cv
            .wait_timeout_while(s, timeout, |s| {
                s.samples.len() < frame && !s.closed && !s.overflowed
            })
            .unwrap_or_else(|e| e.into_inner());
        if s.overflowed {
            Pop::Overflowed
        } else if s.samples.len() >= frame || (s.closed && !s.samples.is_empty()) {
            let n = frame.min(s.samples.len());
            Pop::Data(s.samples.drain(..n).collect())
        } else if s.closed {
            Pop::Closed
        } else {
            Pop::Empty
        }
    }
}

/// Core's handle on a running capture.
pub struct Capture {
    stop: Arc<AtomicBool>,
    pub queue: Arc<LiveQueue>,
}

impl Capture {
    /// Stop recording. Before `Opened` this abandons the capture quietly (no events);
    /// after it, the thread finalizes the WAV and sends `Ended`.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

pub fn start(sid: u64, wav: PathBuf, events: Sender<Event>) -> Capture {
    let stop = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(LiveQueue::default());
    BUSY.store(true, Ordering::Release);
    let (s, q) = (stop.clone(), queue.clone());
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            struct Clear;
            impl Drop for Clear {
                fn drop(&mut self) {
                    BUSY.store(false, Ordering::Release);
                }
            }
            let _clear = Clear;
            let send = |ev| {
                let _ = events.send(Event::Capture { sid, ev });
            };
            if let Err(e) = run(&s, &q, &wav, &send) {
                log::warn!("capture {sid}: {e}");
                q.close();
                if !s.load(Ordering::Acquire) {
                    send(CaptureEvent::Failed(e));
                }
            }
        })
        .expect("spawn capture thread");
    Capture { stop, queue }
}

fn run(
    stop: &AtomicBool,
    queue: &LiveQueue,
    wav: &std::path::Path,
    send: &dyn Fn(CaptureEvent),
) -> Result<(), String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("No microphone")?;
    let config = device
        .default_input_config()
        .map_err(|e| format!("Microphone config: {e}"))?;
    let (rate, channels) = (config.sample_rate(), usize::from(config.channels()).max(1));
    log::info!(
        "capture: {} Hz, {channels} ch, {:?}",
        rate,
        config.sample_format()
    );

    let (tx, rx) = sync_channel::<Vec<f32>>(512);
    let dropped = Arc::new(AtomicU32::new(0));
    let fatal: Arc<Mutex<Option<String>>> = Arc::default();
    let (d, f) = (dropped.clone(), fatal.clone());
    let stream = device
        .build_input_stream::<f32, _, _>(
            config.into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mono = if channels == 1 {
                    data.to_vec()
                } else {
                    data.chunks_exact(channels)
                        .map(|c| c.iter().sum::<f32>() / channels as f32)
                        .collect()
                };
                if tx.try_send(mono).is_err() {
                    d.fetch_add(1, Ordering::Relaxed);
                }
            },
            move |e: cpal::Error| match e.kind() {
                cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged => {
                    log::debug!("capture: {e}")
                }
                _ => {
                    log::warn!("capture stream error: {e}");
                    f.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_or_insert_with(|| e.to_string());
                }
            },
            Some(Duration::from_secs(3)),
        )
        .map_err(|e| format!("Microphone didn't open: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("Microphone didn't start: {e}"))?;

    if stop.load(Ordering::Acquire) {
        return Ok(()); // core gave up while we were opening
    }
    let mut spool = WavSpool::create(wav).map_err(|e| format!("Couldn't write audio: {e}"))?;
    send(CaptureEvent::Opened);

    let mut resampler = Resampler::new(rate);
    let mut out = Vec::with_capacity(1024);
    let mut take = |chunk: &[f32], spool: &mut WavSpool| -> Result<(), String> {
        out.clear();
        resampler.process(chunk, &mut out);
        spool
            .write(&out)
            .map_err(|e| format!("Couldn't write audio: {e}"))?;
        queue.push(&out);
        crate::win::overlay::level(crate::gemini::protocol::rms(&out));
        Ok(())
    };

    let mut reason = None;
    while !stop.load(Ordering::Acquire) {
        if let Some(e) = fatal.lock().unwrap_or_else(|e| e.into_inner()).take() {
            reason = Some(format!("Microphone stopped: {e}"));
            break;
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                if let Err(e) = take(&chunk, &mut spool) {
                    reason = Some(e);
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                reason = Some("Microphone stopped".into());
                break;
            }
        }
    }

    drop(stream); // no more callbacks; drain what's buffered
    while let Ok(chunk) = rx.try_recv() {
        if reason.is_some() || take(&chunk, &mut spool).is_err() {
            break;
        }
    }
    queue.close();
    let duration_ms = spool
        .finish()
        .map_err(|e| format!("Couldn't finish audio: {e}"))?;
    let dropped = dropped.load(Ordering::Relaxed);
    if dropped > 0 {
        log::warn!("capture: {dropped} chunks dropped");
    }
    send(CaptureEvent::Ended {
        duration_ms,
        dropped,
        reason,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_pop_close_overflow() {
        let q = LiveQueue::default();
        assert!(matches!(q.pop(10, Duration::from_millis(1)), Pop::Empty));
        q.push(&[1, 2, 3]);
        assert!(matches!(q.pop(4, Duration::ZERO), Pop::Empty));
        assert!(matches!(q.pop(2, Duration::ZERO), Pop::Data(v) if v == [1, 2]));
        q.close();
        assert!(matches!(q.pop(10, Duration::ZERO), Pop::Data(v) if v == [3]));
        assert!(matches!(q.pop(10, Duration::ZERO), Pop::Closed));

        let q = LiveQueue::default();
        q.push(&vec![0; QUEUE_CAP]);
        q.push(&[1]);
        assert!(matches!(q.pop(10, Duration::ZERO), Pop::Overflowed));
    }
}
