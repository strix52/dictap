//! Per-dictation microphone capture. The cpal callback only downmixes and hands chunks
//! over; this thread resamples, spools the WAV and feeds the Live queue. It never touches
//! the network.

use crate::audio::{RATE, Resampler, WavSpool};
use crate::event::{CaptureEvent, Event, SessionId};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Live queue cap: 15 s of 16 kHz audio. Past this, Live is abandoned and batch uses the WAV.
const QUEUE_CAP: usize = 15 * RATE as usize;

/// At most one capture thread alive; a stuck open keeps this set until it returns.
static BUSY: AtomicBool = AtomicBool::new(false);

struct Reservation<'a> {
    flag: &'a AtomicBool,
    armed: bool,
}
impl Reservation<'_> {
    fn release(&mut self) {
        if self.armed {
            self.armed = false;
            self.flag.store(false, Ordering::Release);
        }
    }
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

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

    /// Whether the queue overflowed (Live fell too far behind and lost audio).
    #[cfg(test)]
    pub fn overflowed(&self) -> bool {
        self.lock().overflowed
    }
}

/// Whether the WAV a capture left behind can be used as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioState {
    /// Header patched and file closed.
    Finalized,
    /// The writer is closed but the header wasn't patched: repair it, never delete it.
    Recoverable,
    /// No file was written.
    Absent,
}

/// What a capture thread leaves behind. Sent exactly once, as the last thing it does.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureReport {
    pub duration_ms: u64,
    /// Microphone chunks lost because the resampler/spool thread fell behind.
    pub dropped_chunks: u32,
    /// Set when capture ended on its own (unplugged device, write error) or never started.
    pub problem: Option<String>,
    pub audio: AudioState,
}

impl CaptureReport {
    pub fn absent(problem: Option<String>) -> CaptureReport {
        CaptureReport {
            duration_ms: 0,
            dropped_chunks: 0,
            problem,
            audio: AudioState::Absent,
        }
    }

    /// Complete audio, nothing lost.
    pub fn clean(&self) -> bool {
        self.audio == AudioState::Finalized && self.problem.is_none() && self.dropped_chunks == 0
    }
}

/// Core's handle on a running capture. Dropping it stops the capture (the thread still
/// finalizes the WAV and reports); moving it between states does not.
pub struct Capture {
    stop: Arc<AtomicBool>,
    pub queue: Arc<LiveQueue>,
}

impl Capture {
    /// Stop recording. Idempotent. The thread finalizes whatever it has and sends `Finished`.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// A handle with no thread behind it, plus an observer of its stop flag.
    #[cfg(test)]
    pub fn detached() -> (Capture, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let cap = Capture {
            stop: stop.clone(),
            queue: Arc::new(LiveQueue::default()),
        };
        (cap, stop)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the capture thread. `Err` means no thread exists (and nothing will be reported);
/// the busy flag is only held by a running thread.
pub fn start(id: SessionId, wav: PathBuf, events: Sender<Event>) -> io::Result<Capture> {
    let stop = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(LiveQueue::default());
    if BUSY
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(io::Error::other("a capture is still shutting down"));
    }
    let (s, q) = (stop.clone(), queue.clone());
    let spawned = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let mut reservation = Reservation {
                flag: &BUSY,
                armed: true,
            };
            let send = |ev| {
                let _ = events.send(Event::Capture { session: id, ev });
            };
            let report = run(&s, &q, &wav, &send);
            log::info!(
                "capture {}: {} ms, dropped {}, audio {:?}, problem {:?}",
                id.0,
                report.duration_ms,
                report.dropped_chunks,
                report.audio,
                report.problem
            );
            // Close the queue before reporting so Live drains and ends first.
            q.close();
            // Release the device claim first: by the time the core sees `Finished`, a new
            // capture can start. (The guard remains as a backstop for a panic in `run`.)
            reservation.release();
            send(CaptureEvent::Finished(report));
        });
    match spawned {
        Ok(_) => Ok(Capture { stop, queue }),
        Err(e) => {
            BUSY.store(false, Ordering::Release);
            Err(e)
        }
    }
}

/// Everything opened before capturing starts.
struct Open {
    stream: cpal::Stream,
    rx: Receiver<Vec<f32>>,
    dropped: Arc<AtomicU32>,
    fatal: Arc<Mutex<Option<String>>>,
    rate: u32,
    spool: WavSpool,
}

/// Opens the microphone and the spool. Checks `stop` between the steps so a cancel during
/// startup unwinds without creating a file. `Err` is the finished report.
fn open(stop: &AtomicBool, wav: &Path) -> Result<Open, CaptureReport> {
    let fail = |e: String| CaptureReport::absent(Some(e));
    let cancelled = || stop.load(Ordering::Acquire);
    let quiet = || CaptureReport::absent(None);

    let device = cpal::default_host()
        .default_input_device()
        .ok_or_else(|| fail("No microphone".into()))?;
    if cancelled() {
        return Err(quiet());
    }
    let config = device
        .default_input_config()
        .map_err(|e| fail(format!("Microphone config: {e}")))?;
    let (rate, channels) = (config.sample_rate(), usize::from(config.channels()).max(1));
    log::info!(
        "capture: {} Hz, {channels} ch, {:?}",
        rate,
        config.sample_format()
    );

    let (tx, rx) = sync_channel::<Vec<f32>>(512);
    let dropped = Arc::new(AtomicU32::new(0));
    let fatal: Arc<Mutex<Option<String>>> = Arc::default();
    let f = fatal.clone();
    let on_error = move |e: cpal::Error| match e.kind() {
        cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged => log::debug!("capture: {e}"),
        _ => {
            log::warn!("capture stream error: {e}");
            f.lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(|| e.to_string());
        }
    };
    let timeout = Some(Duration::from_secs(3));
    let format = config.sample_format();
    let config: cpal::StreamConfig = config.into();
    // Shared-mode WASAPI almost always hands out f32; some drivers offer only 16-bit.
    let stream = match format {
        cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            downmix(channels, tx, dropped.clone(), |s| f32::from(s) / 32768.0),
            on_error,
            timeout,
        ),
        _ => device.build_input_stream::<f32, _, _>(
            config,
            downmix(channels, tx, dropped.clone(), |s| s),
            on_error,
            timeout,
        ),
    }
    .map_err(|e| fail(format!("Microphone didn't open: {e}")))?;
    if cancelled() {
        return Err(quiet());
    }
    stream
        .play()
        .map_err(|e| fail(format!("Microphone didn't start: {e}")))?;
    if cancelled() {
        return Err(quiet()); // core gave up while we were opening
    }
    let spool = match WavSpool::create(wav) {
        Ok(s) => s,
        Err(e) => {
            // A half-created file is the user's audio path: keep it for repair.
            let audio = if wav.exists() {
                AudioState::Recoverable
            } else {
                AudioState::Absent
            };
            return Err(CaptureReport {
                audio,
                ..fail(format!("Couldn't write audio: {e}"))
            });
        }
    };
    Ok(Open {
        stream,
        rx,
        dropped,
        fatal,
        rate,
        spool,
    })
}

/// Records until stopped. After `Opened`, always returns a report for the spool it made.
fn run(
    stop: &AtomicBool,
    queue: &LiveQueue,
    wav: &Path,
    send: &dyn Fn(CaptureEvent),
) -> CaptureReport {
    let Open {
        stream,
        rx,
        dropped,
        fatal,
        rate,
        mut spool,
    } = match open(stop, wav) {
        Ok(o) => o,
        Err(report) => return report,
    };
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

    let mut problem = None;
    while !stop.load(Ordering::Acquire) {
        if let Some(e) = fatal.lock().unwrap_or_else(|e| e.into_inner()).take() {
            problem = Some(format!("Microphone stopped: {e}"));
            break;
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                if let Err(e) = take(&chunk, &mut spool) {
                    problem = Some(e);
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                problem = Some("Microphone stopped".into());
                break;
            }
        }
    }

    drop(stream); // no more callbacks; drain what's buffered
    while problem.is_none() {
        let Ok(chunk) = rx.try_recv() else { break };
        if let Err(e) = take(&chunk, &mut spool) {
            problem = Some(e);
        }
    }
    let dropped_chunks = dropped.load(Ordering::Relaxed);
    match spool.finish() {
        Ok(duration_ms) => CaptureReport {
            duration_ms,
            dropped_chunks,
            problem,
            audio: AudioState::Finalized,
        },
        Err(e) => CaptureReport {
            duration_ms: 0,
            dropped_chunks,
            problem: Some(problem.unwrap_or_else(|| format!("Couldn't finish audio: {e}"))),
            audio: AudioState::Recoverable,
        },
    }
}

/// The cpal callback: averages channels to mono f32 and hands the chunk over.
fn downmix<T: Copy + 'static>(
    channels: usize,
    tx: SyncSender<Vec<f32>>,
    dropped: Arc<AtomicU32>,
    conv: fn(T) -> f32,
) -> impl FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static {
    move |data, _| {
        let mono = data
            .chunks_exact(channels)
            .map(|c| c.iter().map(|&s| conv(s)).sum::<f32>() / channels as f32)
            .collect();
        if tx.try_send(mono).is_err() {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
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
        assert!(!q.overflowed());
        q.push(&[1]);
        assert!(q.overflowed());
        assert!(matches!(q.pop(10, Duration::ZERO), Pop::Overflowed));
    }

    #[test]
    fn dropping_a_capture_stops_it_but_moving_does_not() {
        let (cap, stop) = Capture::detached();
        let moved = Some(cap); // moved between states
        assert!(!stop.load(Ordering::Acquire));
        drop(moved);
        assert!(stop.load(Ordering::Acquire));

        let (cap, stop) = Capture::detached();
        cap.stop();
        cap.stop(); // idempotent
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn report_cleanliness() {
        let ok = CaptureReport {
            duration_ms: 500,
            dropped_chunks: 0,
            problem: None,
            audio: AudioState::Finalized,
        };
        assert!(ok.clean());
        assert!(
            !CaptureReport {
                dropped_chunks: 1,
                ..ok.clone()
            }
            .clean()
        );
        assert!(
            !CaptureReport {
                problem: Some("x".into()),
                ..ok.clone()
            }
            .clean()
        );
        assert!(
            !CaptureReport {
                audio: AudioState::Recoverable,
                ..ok
            }
            .clean()
        );
        assert!(!CaptureReport::absent(None).clean());
    }
    #[test]
    fn old_reservation_drop_cannot_release_new_owner() {
        let flag = AtomicBool::new(true);
        let mut old = Reservation {
            flag: &flag,
            armed: true,
        };
        old.release();
        assert!(
            flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        );
        drop(old);
        assert!(flag.load(Ordering::Acquire));
        let next = Reservation {
            flag: &flag,
            armed: true,
        };
        drop(next);
        assert!(!flag.load(Ordering::Acquire));
    }
    #[test]
    #[ignore = "briefly opens the real default microphone, never uploads audio"]
    fn microphone_start_stop_real() {
        let path = std::env::temp_dir().join(format!(
            "dictap-microphone-smoke-{}.wav",
            std::process::id()
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        let cap = start(SessionId(1), path.clone(), tx).unwrap();
        match rx.recv_timeout(Duration::from_secs(8)).unwrap() {
            Event::Capture {
                ev: CaptureEvent::Opened,
                ..
            } => {}
            _ => panic!("microphone did not open"),
        }
        std::thread::sleep(Duration::from_millis(200));
        cap.stop();
        match rx.recv_timeout(Duration::from_secs(8)).unwrap() {
            Event::Capture {
                ev: CaptureEvent::Finished(report),
                ..
            } => {
                assert!(report.problem.is_none(), "capture error");
                assert_eq!(report.audio, AudioState::Finalized);
            }
            _ => panic!("capture did not stop"),
        }
        assert!(!busy());
        std::fs::remove_file(path).unwrap();
    }
}
