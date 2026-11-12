//! Windows (MediaFoundation) and Linux (V4L2) capture through nokhwa.

use std::sync::{Arc, mpsc};

use anyhow::{Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use nokhwa::Camera;
use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
};

use super::convert::{Packed422, double_lines, packed422_to_bgra, rgba_to_bgra};
use super::{FrameSlot, LatestFrame, VideoDevice, VideoEvent, VideoFormat, sort_formats};

pub fn request_permission(on_complete: impl Fn(bool) + Send + Sync + 'static) {
    nokhwa::nokhwa_initialize(on_complete);
}

pub fn list_devices() -> Vec<VideoDevice> {
    let backend = nokhwa::native_api_backend().unwrap_or(ApiBackend::Auto);
    match nokhwa::query(backend) {
        Ok(devices) => devices
            .into_iter()
            .map(|info| VideoDevice {
                id: info.index().as_string(),
                name: info.human_name(),
            })
            .collect(),
        Err(err) => {
            eprintln!("video device query failed: {err}");
            Vec::new()
        }
    }
}

fn to_video_format(f: &CameraFormat) -> VideoFormat {
    VideoFormat {
        width: f.width(),
        height: f.height(),
        fps: f.frame_rate() as f64,
        pixel: f.format().to_string(),
    }
}

fn supported(format: FrameFormat) -> bool {
    matches!(
        format,
        FrameFormat::YUYV | FrameFormat::MJPEG | FrameFormat::NV12 | FrameFormat::RAWRGB
    )
}

/// True once the owning `Capture` has been dropped.
fn stopped(stop: &mpsc::Receiver<()>) -> bool {
    matches!(stop.try_recv(), Err(mpsc::TryRecvError::Disconnected))
}

pub fn run(
    device: &VideoDevice,
    format: Option<VideoFormat>,
    slot: Arc<FrameSlot>,
    events: &UnboundedSender<VideoEvent>,
    stop: &mpsc::Receiver<()>,
) -> Result<()> {
    let index = match device.id.parse() {
        Ok(i) => CameraIndex::Index(i),
        Err(_) => CameraIndex::String(device.id.clone()),
    };
    let mut camera = Camera::new(
        index,
        RequestedFormat::new::<RgbAFormat>(RequestedFormatType::None),
    )
    .with_context(|| format!("opening {}", device.name))?;

    let camera_formats: Vec<CameraFormat> = camera
        .compatible_camera_formats()
        .context("querying formats")?
        .into_iter()
        .filter(|f| supported(f.format()))
        .collect();
    let mut formats: Vec<VideoFormat> = camera_formats.iter().map(to_video_format).collect();
    sort_formats(&mut formats);

    let wanted = format
        .filter(|f| formats.contains(f))
        .or_else(|| formats.first().cloned())
        .context("device offers no supported pixel format")?;
    let wanted = *camera_formats
        .iter()
        .find(|f| to_video_format(f) == wanted)
        .expect("formats were built from camera_formats");
    let active = camera
        .set_camera_requset(RequestedFormat::new::<RgbAFormat>(
            RequestedFormatType::Exact(wanted),
        ))
        .context("setting format")?;
    camera.open_stream().context("starting stream")?;
    let _ = events.unbounded_send(VideoEvent::Opened {
        formats,
        active: to_video_format(&active),
    });

    let (width, height) = (active.width(), active.height());
    let mut rgba = Vec::new();
    while !stopped(stop) {
        // A stream that stops delivering has almost always been unplugged.
        let Ok(buffer) = camera.frame() else {
            let _ = events.unbounded_send(VideoEvent::Disconnected);
            break;
        };
        if stopped(stop) {
            break;
        }

        // Fresh Vec per frame: RenderImage takes ownership, and the previous
        // frame may still be on screen.
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        match buffer.source_frame_format() {
            FrameFormat::YUYV => {
                // Derive the stride from the buffer length to tolerate padded rows.
                let src = buffer.buffer();
                let stride = (src.len() / height as usize).max(width as usize * 2);
                packed422_to_bgra(src, stride, width, height, Packed422::Yuyv, &mut bgra)
            }
            _ => {
                rgba.resize(bgra.len(), 0);
                buffer
                    .decode_image_to_buffer::<RgbAFormat>(&mut rgba)
                    .context("decoding frame")?;
                rgba_to_bgra(&rgba, &mut bgra);
            }
        }

        let (bgra, frame_height) = double_lines(bgra, width, height);
        let Some(pixels) = RgbaImage::from_raw(width, frame_height, bgra) else {
            continue;
        };
        let frame = LatestFrame {
            image: Arc::new(RenderImage::new([Frame::new(pixels)])),
            captured_at: buffer.capture_timestamp(),
        };
        if !slot.deliver(frame, events) {
            break;
        }
    }
    camera.stop_stream().ok();
    Ok(())
}
