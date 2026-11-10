//! macOS capture through AVFoundation directly.
//!
//! Devices are asked for BGRA output, so AVFoundation does colour conversion
//! in hardware (`420v`, `BGRA`, ...). The exception is packed 4:2:2 (`yuvs`,
//! `2vuy`): some UVC drivers misreport its row stride, which AVFoundation's
//! own conversion can't recover from, so those frames are converted here.
//! Frames go straight from the delegate callback into the frame slot, and
//! late frames are discarded by AVFoundation instead of queued.

use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQoS, DispatchQueue, DispatchQueueAttr};
use futures::channel::mpsc::UnboundedSender;
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceDiscoverySession,
    AVCaptureDeviceFormat, AVCaptureDeviceInput, AVCaptureDevicePosition,
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureDeviceTypeExternal, AVCaptureOutput,
    AVCaptureSession, AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate,
    AVMediaTypeVideo,
};
use objc2_core_foundation::CFString;
use objc2_core_media::{CMClock, CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetDimensions};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_422YpCbCr8, kCVPixelFormatType_422YpCbCr8_yuvs, CVPixelBufferGetDataSize,
    CVPixelBufferGetPixelFormatType,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use super::convert::{Packed422, detect_packed_stride, double_lines, packed422_to_bgra};
use super::{FrameSlot, LatestFrame, VideoDevice, VideoEvent, VideoFormat, sort_formats};

pub fn request_permission(on_complete: impl Fn(bool) + Send + Sync + 'static) {
    let Some(video) = (unsafe { AVMediaTypeVideo }) else {
        return on_complete(false);
    };
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(video) } {
        AVAuthorizationStatus::Authorized => on_complete(true),
        AVAuthorizationStatus::NotDetermined => {
            let block = RcBlock::new(move |granted: Bool| on_complete(granted.as_bool()));
            unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(video, &block) };
        }
        _ => on_complete(false),
    }
}

pub fn list_devices() -> Vec<VideoDevice> {
    let types = unsafe {
        NSArray::from_slice(&[AVCaptureDeviceTypeExternal, AVCaptureDeviceTypeBuiltInWideAngleCamera])
    };
    let session = unsafe {
        AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
            &types,
            AVMediaTypeVideo,
            AVCaptureDevicePosition::Unspecified,
        )
    };
    unsafe { session.devices() }
        .iter()
        .map(|d| VideoDevice {
            id: unsafe { d.uniqueID() }.to_string(),
            name: unsafe { d.localizedName() }.to_string(),
        })
        .collect()
}

/// One selectable mode: a device format plus the frame duration that gives `format.fps`.
struct Mode {
    format: VideoFormat,
    /// Native pixel format code (FourCC).
    pixel_code: u32,
    av_format: Retained<AVCaptureDeviceFormat>,
    frame_duration: CMTime,
}

fn modes(device: &AVCaptureDevice) -> Vec<Mode> {
    let mut modes = Vec::new();
    for av_format in unsafe { device.formats() }.iter() {
        let desc = unsafe { av_format.formatDescription() };
        let dims = unsafe { CMVideoFormatDescriptionGetDimensions(&desc) };
        let pixel_code = unsafe { desc.media_sub_type() };
        let pixel = fourcc(pixel_code);
        for range in unsafe { av_format.videoSupportedFrameRateRanges() }.iter() {
            let (min, max) = unsafe { (range.minFrameRate(), range.maxFrameRate()) };
            // Offer both ends of a range (e.g. 15–30 fps webcams); fixed-rate
            // devices like capture cards report min == max.
            let mut ends = vec![(max, unsafe { range.minFrameDuration() })];
            if (max - min).abs() > 0.01 && min >= 1.0 {
                ends.push((min, unsafe { range.maxFrameDuration() }));
            }
            for (fps, frame_duration) in ends {
                modes.push(Mode {
                    format: VideoFormat {
                        width: dims.width as u32,
                        height: dims.height as u32,
                        fps,
                        pixel: pixel.clone(),
                    },
                    pixel_code,
                    av_format: av_format.clone(),
                    frame_duration,
                });
            }
        }
    }
    modes
}

#[allow(non_upper_case_globals)]
fn packed422(code: u32) -> Option<Packed422> {
    match code {
        kCVPixelFormatType_422YpCbCr8_yuvs => Some(Packed422::Yuyv),
        kCVPixelFormatType_422YpCbCr8 => Some(Packed422::Uyvy),
        _ => None,
    }
}

fn fourcc(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic()) {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("{code:#x}")
    }
}

pub fn run(
    device: &VideoDevice,
    format: Option<VideoFormat>,
    slot: Arc<FrameSlot>,
    events: &UnboundedSender<VideoEvent>,
    stop: &mpsc::Receiver<()>,
) -> Result<()> {
    let av_device = unsafe { AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(&device.id)) }
        .with_context(|| format!("{} is no longer connected", device.name))?;

    let modes = modes(&av_device);
    let mut formats: Vec<VideoFormat> = modes.iter().map(|m| m.format.clone()).collect();
    sort_formats(&mut formats);
    let wanted = format
        .filter(|f| formats.contains(f))
        .or_else(|| formats.first().cloned())
        .with_context(|| format!("{} reports no video formats", device.name))?;
    let mode = modes
        .iter()
        .find(|m| m.format == wanted)
        .expect("formats were built from modes");

    let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&av_device) }
        .map_err(|e| anyhow!("opening {}: {}", device.name, e.localizedDescription()))?;

    let output = unsafe { AVCaptureVideoDataOutput::new() };
    unsafe {
        output.setAlwaysDiscardsLateVideoFrames(true);
        // CFString and NSString are toll-free bridged.
        let key = &*(kCVPixelBufferPixelFormatTypeKey as *const CFString as *const NSString);
        let output_format = match packed422(mode.pixel_code) {
            Some(_) => mode.pixel_code,
            None => kCVPixelFormatType_32BGRA,
        };
        let output_format = NSNumber::new_u32(output_format);
        let settings = NSDictionary::<NSString, AnyObject>::from_slices(&[key], &[&*output_format]);
        output.setVideoSettings(Some(&settings));
    }

    let delegate = FrameDelegate::new(DelegateIvars { slot, events: events.clone() });
    let queue = DispatchQueue::new(
        "frameback.capture",
        Some(&DispatchQueueAttr::with_qos_class(
            DispatchQueueAttr::SERIAL,
            DispatchQoS::UserInteractive,
            0,
        )),
    );
    unsafe {
        output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
    }

    let session = unsafe { AVCaptureSession::new() };
    unsafe {
        session.beginConfiguration();
        if !session.canAddInput(&input) {
            bail!("{} can't be added to a capture session (in use?)", device.name);
        }
        session.addInput(&input);
        if !session.canAddOutput(&output) {
            bail!("video output can't be added to the capture session");
        }
        session.addOutput(&output);
        session.commitConfiguration();

        // Set the mode after the input joins the session (or the session preset
        // overrides it) and hold the lock until running so it sticks.
        av_device
            .lockForConfiguration()
            .map_err(|e| anyhow!("configuring {}: {}", device.name, e.localizedDescription()))?;
        av_device.setActiveFormat(&mode.av_format);
        av_device.setActiveVideoMinFrameDuration(mode.frame_duration);
        av_device.setActiveVideoMaxFrameDuration(mode.frame_duration);
        session.startRunning();
        av_device.unlockForConfiguration();
    }
    let _ = events.unbounded_send(VideoEvent::Opened { formats, active: wanted });

    // Frames arrive on the dispatch queue; this thread just waits to be stopped.
    let _ = stop.recv();
    unsafe {
        session.stopRunning();
        output.setSampleBufferDelegate_queue(None, None);
    }
    Ok(())
}

struct DelegateIvars {
    slot: Arc<FrameSlot>,
    events: UnboundedSender<VideoEvent>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "FramebackFrameDelegate"]
    #[ivars = DelegateIvars]
    struct FrameDelegate;

    unsafe impl NSObjectProtocol for FrameDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for FrameDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn capture_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            if let Some(frame) = unsafe { copy_frame(sample) } {
                let ivars = self.ivars();
                ivars.slot.deliver(frame, &ivars.events);
            }
        }
    }
);

impl FrameDelegate {
    fn new(ivars: DelegateIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

/// Copies a BGRA or packed 4:2:2 sample buffer into a GPUI (BGRA) image.
unsafe fn copy_frame(sample: &CMSampleBuffer) -> Option<LatestFrame> {
    let pixels = unsafe { sample.image_buffer() }?;
    if unsafe { CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return None;
    }
    let width = CVPixelBufferGetWidth(&pixels);
    let height = CVPixelBufferGetHeight(&pixels);
    let stride = CVPixelBufferGetBytesPerRow(&pixels);
    let base = CVPixelBufferGetBaseAddress(&pixels) as *const u8;
    let len = CVPixelBufferGetDataSize(&pixels);
    let bgra = (!base.is_null()).then(|| unsafe { std::slice::from_raw_parts(base, len) }).and_then(|src| {
        match packed422(CVPixelBufferGetPixelFormatType(&pixels)) {
            Some(order) => {
                let stride = detect_packed_stride(src, stride, width as u32, height as u32);
                let mut bgra = vec![0u8; width * height * 4];
                packed422_to_bgra(src, stride, width as u32, height as u32, order, &mut bgra);
                Some(bgra)
            }
            None if stride >= width * 4 => {
                let mut bgra = Vec::with_capacity(width * height * 4);
                for row in src.chunks(stride).take(height) {
                    bgra.extend_from_slice(row.get(..width * 4)?);
                }
                Some(bgra)
            }
            None => None,
        }
    });
    unsafe { CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) };

    let (bgra, height) = double_lines(bgra?, width as u32, height as u32);
    let image = RgbaImage::from_raw(width as u32, height, bgra)?;
    Some(LatestFrame {
        image: Arc::new(RenderImage::new([Frame::new(image)])),
        captured_at: unsafe { captured_at(sample) },
    })
}

/// Converts the sample's presentation time (host clock) to wall-clock time.
unsafe fn captured_at(sample: &CMSampleBuffer) -> Option<Duration> {
    let pts = unsafe { sample.presentation_time_stamp().seconds() };
    let now = unsafe { CMClock::host_time_clock().time().seconds() };
    let age = now - pts;
    // Anything outside this window isn't on the host clock.
    if !(0.0..5.0).contains(&age) {
        return None;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .checked_sub(Duration::from_secs_f64(age))
}
