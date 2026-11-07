//! Video capture: one thread per open device, publishing only the newest frame.
//!
//! Latency rule: frames are never queued. The backend overwrites a single slot,
//! and the UI picks up whatever is newest when it wakes. A slow UI frame drops
//! video frames instead of falling behind.

mod convert;

#[cfg(target_os = "macos")]
#[path = "video/avfoundation.rs"]
mod backend;
#[cfg(not(target_os = "macos"))]
#[path = "video/nokhwa_capture.rs"]
mod backend;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::channel::mpsc::UnboundedSender;
use gpui::RenderImage;

pub use backend::{list_devices, request_permission};

#[derive(Clone, Debug)]
pub struct VideoDevice {
    /// Backend-specific identifier (AVFoundation unique ID, or nokhwa index).
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Native pixel format as the device names it, e.g. `yuvs`, `420v`, `MJPEG`.
    pub pixel: String,
}

impl VideoFormat {
    pub fn label(&self) -> String {
        let fps = if self.fps.fract().abs() < 0.005 {
            format!("{}", self.fps.round())
        } else {
            format!("{:.2}", self.fps)
        };
        format!("{}×{} @ {fps} {}", self.width, self.height, self.pixel)
    }
}

/// Sorts formats best-first: widest, then tallest, then highest frame rate.
/// Width first so 1920×1080 beats square or portrait sensor modes with more pixels.
fn sort_formats(formats: &mut Vec<VideoFormat>) {
    formats.sort_by(|a, b| {
        b.width
            .cmp(&a.width)
            .then(b.height.cmp(&a.height))
            .then(b.fps.total_cmp(&a.fps))
    });
    formats.dedup();
}

pub struct LatestFrame {
    pub image: Arc<RenderImage>,
    /// Wall-clock capture time, when the backend reports one.
    pub captured_at: Option<Duration>,
}

pub enum VideoEvent {
    /// Result of the OS camera-permission request.
    Permission(bool),
    Opened {
        formats: Vec<VideoFormat>,
        active: VideoFormat,
    },
    /// A new frame is waiting in the slot. Sent at most once until the UI takes it.
    FrameReady,
    Error(String),
}

/// Shared between the capture thread and the UI.
#[derive(Default)]
pub struct FrameSlot {
    latest: Mutex<Option<LatestFrame>>,
    pending: AtomicBool,
    /// Copy of each frame for the recorder, if one is attached.
    tap: Mutex<Option<mpsc::SyncSender<TapFrame>>>,
    /// Frames the recorder couldn't keep up with.
    tap_dropped: AtomicU64,
    /// Every frame the backend has delivered, for measuring the real source rate.
    delivered: AtomicU64,
}

/// A frame handed to the recorder, stamped on the shared session clock.
pub struct TapFrame {
    pub image: Arc<RenderImage>,
    pub at: Instant,
}

impl FrameSlot {
    pub fn take(&self) -> Option<LatestFrame> {
        self.pending.store(false, Ordering::Release);
        self.latest.lock().unwrap().take()
    }

    /// Returns true if the UI needs a wake-up (it hasn't been told about an earlier frame yet).
    fn publish(&self, frame: LatestFrame) -> bool {
        *self.latest.lock().unwrap() = Some(frame);
        !self.pending.swap(true, Ordering::AcqRel)
    }

    /// Publishes a frame and wakes the UI if needed. Returns false once the UI is gone.
    fn deliver(&self, frame: LatestFrame, events: &UnboundedSender<VideoEvent>) -> bool {
        self.delivered.fetch_add(1, Ordering::Relaxed);
        self.feed_tap(&frame);
        !self.publish(frame) || events.unbounded_send(VideoEvent::FrameReady).is_ok()
    }

    /// Publishes a frame that didn't come from capture (DVR playback).
    /// Returns true if the UI needs a wake-up.
    pub fn show(&self, frame: LatestFrame) -> bool {
        self.publish(frame)
    }

    /// Attaches (or with None, detaches) the recorder's frame channel.
    pub fn set_tap(&self, tap: Option<mpsc::SyncSender<TapFrame>>) {
        if tap.is_some() {
            self.tap_dropped.store(0, Ordering::Relaxed);
        }
        *self.tap.lock().unwrap() = tap;
    }

    pub fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Relaxed)
    }

    pub fn tap_dropped(&self) -> u64 {
        self.tap_dropped.load(Ordering::Relaxed)
    }

    /// Never blocks: a full queue drops the frame from the recording only.
    fn feed_tap(&self, frame: &LatestFrame) {
        let tap = self.tap.lock().unwrap();
        let Some(tap) = tap.as_ref() else { return };
        let at = frame
            .captured_at
            .and_then(age)
            .and_then(|age| Instant::now().checked_sub(age))
            .unwrap_or_else(Instant::now);
        if tap.try_send(TapFrame { image: frame.image.clone(), at }).is_err() {
            self.tap_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A running capture. Dropping it stops the capture thread.
pub struct Capture {
    _stop: mpsc::Sender<()>,
    done: mpsc::Receiver<()>,
}

impl Capture {
    /// Starts capturing. `previous` is stopped first; the new thread waits briefly
    /// for it to release the device.
    pub fn start(
        device: VideoDevice,
        format: Option<VideoFormat>,
        slot: Arc<FrameSlot>,
        events: UnboundedSender<VideoEvent>,
        previous: Option<Capture>,
    ) -> Capture {
        let (stop_tx, stop) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let previous_done = previous.map(|p| p.done);

        thread::Builder::new()
            .name("frameback-capture".into())
            .spawn(move || {
                if let Some(previous_done) = previous_done {
                    // A device that stopped sending frames can leave the old thread blocked;
                    // don't wait on it forever.
                    let _ = previous_done.recv_timeout(Duration::from_secs(1));
                }
                if let Err(err) = backend::run(&device, format, slot, &events, &stop) {
                    let _ = events.unbounded_send(VideoEvent::Error(format!("{err:#}")));
                }
                let _ = done_tx.send(());
            })
            .expect("spawn capture thread");

        Capture { _stop: stop_tx, done }
    }
}

/// Age of a frame, given its wall-clock capture time.
pub fn age(captured_at: Duration) -> Option<Duration> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .checked_sub(captured_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Frame, RgbaImage};

    #[test]
    fn slot_wakes_ui_once_until_taken() {
        let slot = FrameSlot::default();
        let frame = || LatestFrame {
            image: Arc::new(RenderImage::new([Frame::new(RgbaImage::new(1, 1))])),
            captured_at: None,
        };
        assert!(slot.publish(frame()));
        assert!(!slot.publish(frame()), "second frame before take must not re-notify");
        assert!(slot.take().is_some());
        assert!(slot.take().is_none());
        assert!(slot.publish(frame()));
    }

    #[test]
    #[ignore = "opens real capture devices; run with -- --ignored --nocapture"]
    fn every_device_delivers_a_frame() {
        use futures::channel::mpsc::TryRecvError;
        use std::time::Instant;
        let mut failures = Vec::new();
        for device in list_devices() {
            let slot = Arc::new(FrameSlot::default());
            let (tx, mut rx) = futures::channel::mpsc::unbounded();
            let capture = Capture::start(device.clone(), None, slot.clone(), tx, None);
            let deadline = Instant::now() + Duration::from_secs(5);
            let result = loop {
                match rx.try_recv() {
                    Ok(VideoEvent::Opened { active, formats }) => {
                        println!("{}: opened {} ({} modes)", device.name, active.label(), formats.len())
                    }
                    Ok(VideoEvent::FrameReady) => {
                        let frame = slot.take().unwrap();
                        let size = frame.image.size(0);
                        let latency = frame.captured_at.and_then(age);
                        // Set FRAMEBACK_FRAME_DIR to save each device's frame for a visual check.
                        if let Some(dir) = std::env::var_os("FRAMEBACK_FRAME_DIR") {
                            let path = std::path::Path::new(&dir).join(format!("{}.png", device.name));
                            crate::ui::save_png(&frame.image, &path).unwrap();
                        }
                        break Ok(format!("{}×{} frame, capture→app {latency:?}", size.width.0, size.height.0));
                    }
                    Ok(VideoEvent::Error(err)) => break Err(err),
                    Ok(_) => {}
                    Err(TryRecvError::Closed) => break Err("event channel closed".into()),
                    Err(TryRecvError::Empty) if Instant::now() > deadline => {
                        break Err("no frame within 5 s".into());
                    }
                    Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(10)),
                }
            };
            drop(capture);
            match result {
                Ok(msg) => println!("{}: {msg}", device.name),
                Err(err) => failures.push(format!("{}: {err}", device.name)),
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn fractional_frame_rates_survive_labels() {
        let f = |fps| VideoFormat { width: 1920, height: 1080, fps, pixel: "yuvs".into() };
        assert_eq!(f(59.940179700658746).label(), "1920×1080 @ 59.94 yuvs");
        assert_eq!(f(60.0).label(), "1920×1080 @ 60 yuvs");
    }

    #[test]
    fn formats_sort_by_width_then_fps() {
        let f = |w, h, fps| VideoFormat { width: w, height: h, fps, pixel: "420v".into() };
        let mut formats = vec![
            f(1280, 720, 60.0),
            f(1552, 1552, 30.0),
            f(1920, 1080, 30.0),
            f(1920, 1080, 59.94),
        ];
        sort_formats(&mut formats);
        assert_eq!(
            formats,
            vec![f(1920, 1080, 59.94), f(1920, 1080, 30.0), f(1552, 1552, 30.0), f(1280, 720, 60.0)]
        );
    }
}
