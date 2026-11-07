use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{
    App, Bounds, ClickEvent, Context, CursorStyle, FocusHandle, Hsla, IntoElement, MouseButton,
    ObjectFit, Pixels, RenderImage, SharedString, Task, Window, actions, canvas, deferred, div,
    img, prelude::*, px, rgb, rgba,
};

use crate::audio::{self, AudioDevice, AudioTap, Monitor, Volume};
use crate::update::{self, Release};
use crate::record::{Codec, Pipeline, PipelineEvent, Player, PlayerEvent, Store, effective_fps};
use crate::video::{self, Capture, FrameSlot, VideoDevice, VideoEvent, VideoFormat};

actions!(
    frameback,
    [
        ToggleFullscreen,
        ExitFullscreen,
        Screenshot,
        ToggleMute,
        ToggleStats,
        ToggleRecord,
        TogglePlay,
        StepBack,
        StepForward,
        JumpBack,
        JumpForward,
        GoLive,
        ToggleClipMode,
        CopyClip,
        CopyFrame,
        MarkStart,
        MarkEnd,
        SaveClip,
        Quit
    ]
);

const CONTROLS_HIDE_AFTER: Duration = Duration::from_millis(2500);

#[derive(Clone, Copy, PartialEq)]
enum Menu {
    Video,
    Format,
    Audio,
    Codec,
    Dvr,
    Step,
}

enum Update {
    Available(Release),
    Downloading(Release),
    /// Downloaded and verified; installs when Frameback quits.
    Ready(Release, PathBuf),
}

const JUMP_SECS: f64 = 5.0;
const DVR_WINDOWS_MIN: [u64; 5] = [1, 5, 10, 30, 60];
const STEP_SIZES: [i64; 5] = [1, 5, 10, 30, 60];

/// The always-on encoder and its rewind buffer.
struct Dvr {
    pipeline: Pipeline,
    store: Option<Store>,
    encoder: Option<String>,
}

struct Recording {
    started: Instant,
}

/// Watching the past: a player shows buffered frames instead of live ones.
struct Timeshift {
    player: Player,
    time: f64,
    playing: bool,
    /// The timeline's span, frozen when timeshift starts so it holds still
    /// while capture keeps extending the buffer. Grows only when playback
    /// moves past its end.
    range: (f64, f64),
    /// Clip mode: marking and saving a clip (a sub-mode of review).
    editing_clip: bool,
    /// Clip being edited: first and last frame (session seconds).
    clip_start: Option<f64>,
    clip_end: Option<f64>,
}

/// The span the timeline shows: the frozen range while reviewing (minus
/// anything pruned since), or the whole buffer when live.
fn visible_range(frozen: Option<(f64, f64)>, buffered: (f64, f64)) -> (f64, f64) {
    match frozen {
        Some((start, end)) => (start.max(buffered.0), end.max(buffered.0)),
        None => buffered,
    }
}

#[derive(Default)]
struct Stats {
    /// `FrameSlot::delivered` at `since`.
    delivered: u64,
    since: Option<Instant>,
    /// Rate frames arrive from the device (not how often the UI redraws).
    fps: f32,
    latency_ms: Option<f32>,
}

pub struct Frameback {
    focus: FocusHandle,
    events: UnboundedSender<VideoEvent>,
    slot: Arc<FrameSlot>,

    video_devices: Vec<VideoDevice>,
    video_selected: Option<usize>,
    formats: Vec<VideoFormat>,
    active_format: Option<VideoFormat>,
    capture: Option<Capture>,

    frame: Option<Arc<RenderImage>>,
    /// Replaced frames, freed from the GPU atlas one render later so a frame
    /// still in flight on the GPU never has its texture reused under it.
    retired: Vec<Arc<RenderImage>>,
    to_free: Vec<Arc<RenderImage>>,

    audio_devices: Vec<AudioDevice>,
    audio_selected: Option<usize>,
    monitor: Option<Monitor>,
    volume: Volume,
    muted_volume: Option<f32>,

    codec: Codec,
    dvr_window_min: u64,
    dvr: Option<Dvr>,
    recording: Option<Recording>,
    timeshift: Option<Timeshift>,
    audio_tap: AudioTap,
    playback_slot: Arc<FrameSlot>,
    pipeline_events: UnboundedSender<PipelineEvent>,
    player_events: UnboundedSender<PlayerEvent>,
    timeline_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    scrubbing: bool,
    /// Frames moved per ←/→ press or step button.
    step_frames: i64,
    update: Option<Update>,

    menu: Option<Menu>,
    controls_visible: bool,
    toolbar_hovered: bool,
    hide_controls: Option<Task<()>>,
    show_stats: bool,
    stats: Stats,
    toast: Option<SharedString>,
    clear_toast: Option<Task<()>>,
    _events_task: Task<()>,
    _pipeline_task: Task<()>,
    _player_task: Task<()>,
    _quit: gpui::Subscription,
}

impl Frameback {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (events, mut rx) = unbounded();
        let events_task = cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                if this.update(cx, |this, cx| this.on_video_event(event, cx)).is_err() {
                    break;
                }
            }
        });

        let (pipeline_events, mut pipeline_rx) = unbounded();
        let pipeline_task = cx.spawn(async move |this, cx| {
            while let Some(event) = pipeline_rx.next().await {
                if this.update(cx, |this, cx| this.on_pipeline_event(event, cx)).is_err() {
                    break;
                }
            }
        });
        let (player_events, mut player_rx) = unbounded();
        let player_task = cx.spawn(async move |this, cx| {
            while let Some(event) = player_rx.next().await {
                if this.update(cx, |this, cx| this.on_player_event(event, cx)).is_err() {
                    break;
                }
            }
        });
        // Finalize an in-progress recording before the app exits.
        let quit = cx.on_app_quit(|this, cx| {
            let dvr = this.detach_pipeline();
            cx.background_executor().spawn(async move {
                if let Some(dvr) = dvr {
                    dvr.pipeline.join();
                }
            })
        });

        // macOS asks for camera permission here; elsewhere this completes immediately.
        let permission = events.clone();
        video::request_permission(move |granted| {
            let _ = permission.unbounded_send(VideoEvent::Permission(granted));
        });

        let focus = cx.focus_handle();
        window.focus(&focus);

        // Only installed builds update (not ones run from cargo's target folder).
        if update::install_path().is_some() {
            let check = cx.background_executor().spawn(async { update::check() });
            cx.spawn(async move |this, cx| {
                if let Ok(Some(release)) = check.await {
                    let _ = this.update(cx, |this, cx| {
                        this.update = Some(Update::Available(release));
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        // Left by the install script if the last update couldn't be installed.
        let update_failure = update::take_failure_notice()
            .map(|why| SharedString::from(format!("The last update failed: {why}")));

        Self {
            focus,
            events,
            slot: Arc::default(),
            video_devices: Vec::new(),
            video_selected: None,
            formats: Vec::new(),
            active_format: None,
            capture: None,
            frame: None,
            retired: Vec::new(),
            to_free: Vec::new(),
            audio_devices: audio::list_inputs(),
            audio_selected: None,
            monitor: None,
            volume: Volume::new(1.0),
            muted_volume: None,
            codec: Codec::Hevc,
            dvr_window_min: 1,
            dvr: None,
            recording: None,
            timeshift: None,
            audio_tap: AudioTap::default(),
            playback_slot: Arc::default(),
            pipeline_events,
            player_events,
            timeline_bounds: Rc::default(),
            scrubbing: false,
            step_frames: 1,
            update: None,
            menu: None,
            controls_visible: true,
            toolbar_hovered: false,
            hide_controls: None,
            show_stats: false,
            stats: Stats::default(),
            toast: update_failure,
            clear_toast: None,
            _events_task: events_task,
            _pipeline_task: pipeline_task,
            _player_task: player_task,
            _quit: quit,
        }
    }

    fn on_video_event(&mut self, event: VideoEvent, cx: &mut Context<Self>) {
        match event {
            VideoEvent::Permission(true) => {
                self.video_devices = video::list_devices();
                if self.video_devices.is_empty() {
                    self.show_toast("No capture devices found", cx);
                } else {
                    self.select_video(0, cx);
                }
            }
            VideoEvent::Permission(false) => {
                self.show_toast("Camera access denied — allow it in system privacy settings", cx);
            }
            VideoEvent::Opened { formats, active } => {
                self.formats = formats;
                self.active_format = Some(active);
                self.auto_select_audio(cx);
            }
            VideoEvent::FrameReady => {
                let Some(latest) = self.slot.take() else { return };
                self.stats.latency_ms = latest
                    .captured_at
                    .and_then(video::age)
                    .map(|d| d.as_secs_f32() * 1000.0);
                self.count_frame(cx);
                // In timeshift the live feed keeps recording but isn't shown.
                if self.timeshift.is_none() {
                    self.display(latest.image);
                }
                cx.notify();
            }
            VideoEvent::Error(err) => self.show_toast(err, cx),
        }
    }

    fn display(&mut self, image: Arc<RenderImage>) {
        if let Some(old) = self.frame.replace(image) {
            self.retired.push(old);
        }
    }

    fn count_frame(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let delivered = self.slot.delivered();
        let Some(since) = self.stats.since else {
            self.stats.since = Some(now);
            self.stats.delivered = delivered;
            return;
        };
        let elapsed = now - since;
        if elapsed >= Duration::from_millis(500) {
            let frames = delivered - self.stats.delivered;
            self.stats.fps = frames as f32 / elapsed.as_secs_f32();
            self.stats.delivered = delivered;
            self.stats.since = Some(now);
            // The pipeline needs the real frame rate, so it starts after the first measurement.
            if self.dvr.is_none() {
                self.start_pipeline(cx);
            }
        }
    }

    /// True (and explains why) if `action` would discard the buffer being reviewed.
    fn blocked_in_review(&mut self, action: &str, cx: &mut Context<Self>) -> bool {
        if self.timeshift.is_none() {
            return false;
        }
        self.show_toast(format!("Go LIVE (L) before {action}: it would clear the rewind buffer"), cx);
        true
    }

    fn select_video(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.blocked_in_review("switching devices", cx) {
            return;
        }
        self.video_selected = Some(index);
        self.formats.clear();
        self.active_format = None;
        self.start_capture(None, cx);
    }

    fn select_format(&mut self, format: VideoFormat, cx: &mut Context<Self>) {
        if self.blocked_in_review("changing format", cx) {
            return;
        }
        self.start_capture(Some(format), cx);
    }

    fn start_capture(&mut self, format: Option<VideoFormat>, cx: &mut Context<Self>) {
        let Some(device) = self.video_selected.map(|i| self.video_devices[i].clone()) else {
            return;
        };
        // The pipeline's frame size and rate are fixed; it restarts once the new
        // source's rate has been measured.
        self.stop_pipeline("capture source changed", cx);
        self.stats = Stats::default();
        self.capture = Some(Capture::start(
            device,
            format,
            self.slot.clone(),
            self.events.clone(),
            self.capture.take(),
        ));
        cx.notify();
    }

    /// Capture cards usually expose audio under the same name as their video.
    fn auto_select_audio(&mut self, cx: &mut Context<Self>) {
        if self.monitor.is_some() {
            return;
        }
        let Some(video) = self.video_selected.map(|i| self.video_devices[i].name.to_lowercase())
        else {
            return;
        };
        let matched = self.audio_devices.iter().position(|a| {
            let a = a.name.to_lowercase();
            a.contains(&video) || video.contains(&a)
        });
        if let Some(index) = matched {
            self.select_audio(Some(index), cx);
        }
    }

    fn select_audio(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        if self.blocked_in_review("changing audio input", cx) {
            return;
        }
        self.monitor = None;
        self.audio_selected = index;
        if let Some(device) = index.map(|i| &self.audio_devices[i]) {
            match Monitor::start(device, self.volume.clone(), self.audio_tap.clone()) {
                Ok(monitor) => self.monitor = Some(monitor),
                Err(err) => {
                    self.audio_selected = None;
                    self.show_toast(format!("Audio: {err:#}"), cx);
                }
            }
        }
        // The pipeline's audio stream is fixed at start.
        self.restart_pipeline("audio input changed", cx);
        cx.notify();
    }

    fn show_toast(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some(message.into());
        self.clear_toast = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            let _ = this.update(cx, |this, cx| {
                this.toast = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn reveal_controls(&mut self, cx: &mut Context<Self>) {
        if !self.controls_visible {
            self.controls_visible = true;
            cx.notify();
        }
        self.hide_controls = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CONTROLS_HIDE_AFTER).await;
            let _ = this.update(cx, |this, cx| {
                if this.menu.is_none() && !this.toolbar_hovered {
                    this.controls_visible = false;
                    cx.notify();
                }
            });
        }));
    }

    fn toggle_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        self.menu = if self.menu == Some(menu) { None } else { Some(menu) };
        cx.notify();
    }

    fn toggle_fullscreen(&mut self, _: &ToggleFullscreen, window: &mut Window, _: &mut Context<Self>) {
        window.toggle_fullscreen();
    }

    fn exit_fullscreen(&mut self, _: &ExitFullscreen, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        } else if self.editing_clip() {
            self.toggle_clip_mode(cx);
        } else if window.is_fullscreen() {
            window.toggle_fullscreen();
        }
    }

    fn toggle_mute(&mut self, _: &ToggleMute, _: &mut Window, cx: &mut Context<Self>) {
        match self.muted_volume.take() {
            Some(v) => self.volume.set(v),
            None => {
                self.muted_volume = Some(self.volume.get());
                self.volume.set(0.0);
            }
        }
        cx.notify();
    }

    // --- Pipeline (DVR buffer + recording) ---------------------------------

    fn start_pipeline(&mut self, cx: &mut Context<Self>) {
        let Some(mut format) = self.active_format.clone() else { return };
        // Some devices deliver fewer frames than they advertise (an Elgato
        // HD60 S+ sends 29.97 while claiming 59.94); encode at the real rate.
        let measured = (self.stats.fps > 0.0).then_some(self.stats.fps as f64);
        format.fps = effective_fps(format.fps, measured);
        let audio_rate = self.monitor.as_ref().map(|m| m.sample_rate);
        let window = Duration::from_secs(self.dvr_window_min * 60);
        let (pipeline, taps) =
            Pipeline::start(self.codec, format, audio_rate, window, self.pipeline_events.clone());
        self.slot.set_tap(Some(taps.video));
        self.audio_tap.set(taps.audio);
        self.dvr = Some(Dvr { pipeline, store: None, encoder: None });
        cx.notify();
    }

    /// Disconnects the pipeline's inputs; it then finalizes any recording and exits.
    fn detach_pipeline(&mut self) -> Option<Dvr> {
        self.timeshift = None;
        self.recording = None;
        self.slot.set_tap(None);
        self.audio_tap.set(None);
        self.dvr.take()
    }

    fn stop_pipeline(&mut self, reason: &str, cx: &mut Context<Self>) {
        if self.recording.is_some() {
            self.show_toast(format!("Recording stopped: {reason}"), cx);
        } else if self.buffered().is_some_and(|(a, b)| b - a >= 1.0) {
            self.show_toast(format!("Rewind buffer cleared: {reason}"), cx);
        }
        if let Some(dvr) = self.detach_pipeline() {
            cx.background_executor().spawn(async move { dvr.pipeline.join() }).detach();
        }
        cx.notify();
    }

    fn restart_pipeline(&mut self, reason: &str, cx: &mut Context<Self>) {
        if self.dvr.is_some() {
            self.stop_pipeline(reason, cx);
            self.start_pipeline(cx);
        }
    }

    fn on_pipeline_event(&mut self, event: PipelineEvent, cx: &mut Context<Self>) {
        match event {
            PipelineEvent::Ready { encoder, store } => {
                if let Some(dvr) = &mut self.dvr {
                    dvr.store = Some(store);
                    dvr.encoder = Some(encoder);
                }
            }
            PipelineEvent::Recording { path } => {
                self.show_toast(format!("Recording to {}", path.display()), cx);
            }
            PipelineEvent::Saved { path, frames } => {
                let mut message = format!("Saved {} ({frames} frames", path.display());
                let dropped = self.slot.tap_dropped();
                if dropped > 0 {
                    message += &format!(", {dropped} dropped: encoder too slow");
                }
                self.show_toast(message + ")", cx);
            }
            PipelineEvent::Error(err) => {
                self.recording = None;
                self.show_toast(err, cx);
            }
            PipelineEvent::Failed(err) => {
                self.stop_pipeline("encoder stopped", cx);
                self.show_toast(err, cx);
            }
        }
        cx.notify();
    }

    fn toggle_record(&mut self, _: &ToggleRecord, _: &mut Window, cx: &mut Context<Self>) {
        let Some(dvr) = &self.dvr else {
            self.show_toast("Nothing to record yet", cx);
            return;
        };
        if self.recording.take().is_some() {
            dvr.pipeline.stop_recording();
        } else if self.timeshift.is_some() {
            // Recording takes the live feed, not what's being reviewed.
            self.show_toast("Go LIVE (L) to start recording", cx);
            return;
        } else {
            dvr.pipeline.record(output_path(MOVIES_DIR, "frameback", self.codec.extension()), Instant::now());
            self.recording = Some(Recording { started: Instant::now() });
        }
        cx.notify();
    }

    fn select_codec(&mut self, codec: Codec, cx: &mut Context<Self>) {
        if self.codec != codec && !self.blocked_in_review("changing codec", cx) {
            self.codec = codec;
            self.restart_pipeline("codec changed", cx);
        }
    }

    fn select_dvr_window(&mut self, minutes: u64, cx: &mut Context<Self>) {
        if self.dvr_window_min != minutes && !self.blocked_in_review("resizing the buffer", cx) {
            self.dvr_window_min = minutes;
            self.restart_pipeline("rewind buffer resized", cx);
        }
    }

    // --- Timeshift ------------------------------------------------------------

    fn buffered(&self) -> Option<(f64, f64)> {
        self.dvr.as_ref()?.store.as_ref()?.video_range()
    }

    /// Switches from live to the buffer, showing `time` (default: newest frame).
    fn enter_timeshift(&mut self, time: Option<f64>, cx: &mut Context<Self>) -> bool {
        if self.timeshift.is_some() {
            return true;
        }
        let Some(store) = self.dvr.as_ref().and_then(|d| d.store.clone()) else { return false };
        let Some(range) = store.video_range() else { return false };
        let audio = self.monitor.as_ref().map(|m| m.playback.clone());
        let player =
            Player::start(store, self.playback_slot.clone(), audio, self.player_events.clone());
        let time = time.unwrap_or(range.1);
        player.seek(time);
        // Nothing new goes into (or ages out of) the buffer while it's reviewed.
        if let Some(dvr) = &self.dvr {
            dvr.pipeline.set_frozen(true);
        }
        self.timeshift = Some(Timeshift {
            player,
            time,
            playing: false,
            range,
            editing_clip: false,
            clip_start: None,
            clip_end: None,
        });
        cx.notify();
        true
    }

    fn go_live(&mut self, cx: &mut Context<Self>) {
        if self.timeshift.take().is_some() {
            if let Some(dvr) = &self.dvr {
                dvr.pipeline.set_frozen(false);
            }
            cx.notify();
        }
    }

    fn editing_clip(&self) -> bool {
        self.timeshift.as_ref().is_some_and(|ts| ts.editing_clip)
    }

    /// Opens clip mode (entering review at the newest frame if live), or
    /// closes it and drops the marks.
    fn toggle_clip_mode(&mut self, cx: &mut Context<Self>) {
        if let Some(ts) = &mut self.timeshift
            && ts.editing_clip
        {
            ts.editing_clip = false;
            ts.clip_start = None;
            ts.clip_end = None;
        } else if self.enter_timeshift(None, cx)
            && let Some(ts) = &mut self.timeshift
        {
            ts.editing_clip = true;
        }
        cx.notify();
    }

    /// Marks the frame on screen as the clip's first (`start`) or last frame.
    fn mark(&mut self, start: bool, cx: &mut Context<Self>) {
        let Some(ts) = self.timeshift.as_mut().filter(|ts| ts.editing_clip) else {
            self.show_toast("Open Clip (C) to mark a clip", cx);
            return;
        };
        let t = ts.time;
        if start {
            ts.clip_start = Some(t);
            ts.clip_end = ts.clip_end.filter(|&end| end >= t);
        } else {
            ts.clip_end = Some(t);
            ts.clip_start = ts.clip_start.filter(|&start| start <= t);
        }
        cx.notify();
    }

    /// The clip to save: the marked range, defaulting to the buffer's ends.
    fn clip_range(&self) -> Option<(f64, f64)> {
        let ts = self.timeshift.as_ref().filter(|ts| ts.editing_clip)?;
        let (oldest, newest) = self.timeline_range()?;
        Some((ts.clip_start.unwrap_or(oldest), ts.clip_end.unwrap_or(newest)))
    }

    /// Writes the clip to `dir` in the background. None (with a hint) outside clip mode.
    fn export_clip(
        &mut self,
        dir: &str,
        verb: &str,
        cx: &mut Context<Self>,
    ) -> Option<(PathBuf, Task<anyhow::Result<u64>>)> {
        let (Some((from, to)), Some(store)) =
            (self.clip_range(), self.dvr.as_ref().and_then(|d| d.store.clone()))
        else {
            self.show_toast(format!("Open Clip (C) to {verb} a clip"), cx);
            return None;
        };
        let codec = self.codec;
        let path = output_path(dir, "frameback-clip", codec.extension());
        let task = cx.background_executor().spawn({
            let path = path.clone();
            async move { crate::record::save_clip(&store, codec, from, to, path) }
        });
        Some((path, task))
    }

    fn save_clip(&mut self, cx: &mut Context<Self>) {
        let Some((path, task)) = self.export_clip(MOVIES_DIR, "save", cx) else { return };
        self.show_toast("Saving clip…", cx);
        cx.spawn(async move |this, cx| {
            let message = match task.await {
                Ok(frames) => format!("Saved clip {} ({frames} frames)", path.display()),
                Err(err) => format!("Saving clip failed: {err:#}"),
            };
            let _ = this.update(cx, |this, cx| this.show_toast(message, cx));
        })
        .detach();
    }

    /// Exports the clip to a temp file and puts that file on the clipboard.
    fn copy_clip(&mut self, cx: &mut Context<Self>) {
        let dir = copied_clips_dir();
        let Some((path, task)) = self.export_clip(&dir, "copy", cx) else { return };
        self.show_toast("Copying clip…", cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let message = match result {
                    Ok(frames) if crate::clipboard::copy_file(&path) => {
                        format!("Copied clip ({frames} frames). Paste it anywhere that takes files")
                    }
                    Ok(_) => {
                        let text = path.to_string_lossy().into_owned();
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                        "Copied the clip's file path (copying files isn't supported here yet)".into()
                    }
                    Err(err) => format!("Copying clip failed: {err:#}"),
                };
                this.show_toast(message, cx);
            });
        })
        .detach();
    }

    /// Puts the frame on screen on the clipboard as a PNG.
    fn copy_frame(&mut self, cx: &mut Context<Self>) {
        let frame = self.frame.clone().filter(|_| self.editing_clip());
        let Some(frame) = frame else {
            self.show_toast("Open Clip (C) to copy a frame", cx);
            return;
        };
        let task = cx.background_executor().spawn(async move { encode_png(&frame) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let message = match result {
                    Ok(png) => {
                        let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, png);
                        cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image));
                        "Copied frame".to_string()
                    }
                    Err(err) => format!("Copying frame failed: {err:#}"),
                };
                this.show_toast(message, cx);
            });
        })
        .detach();
    }

    /// The timeline's current span (see `visible_range`).
    fn timeline_range(&self) -> Option<(f64, f64)> {
        Some(visible_range(self.timeshift.as_ref().map(|ts| ts.range), self.buffered()?))
    }

    fn seek(&mut self, time: f64, cx: &mut Context<Self>) {
        let Some((oldest, newest)) = self.buffered() else { return };
        let time = time.clamp(oldest, newest);
        if !self.enter_timeshift(Some(time), cx) {
            return;
        }
        if let Some(ts) = &mut self.timeshift {
            ts.player.seek(time);
            ts.time = time;
        }
        cx.notify();
    }

    /// Extends the frozen range once playback has moved past its end.
    fn follow_playhead(&mut self) {
        let newest = self.buffered().map(|(_, newest)| newest);
        if let (Some(ts), Some(newest)) = (&mut self.timeshift, newest)
            && ts.time > ts.range.1
        {
            ts.range.1 = newest;
        }
    }

    fn on_player_event(&mut self, event: PlayerEvent, cx: &mut Context<Self>) {
        match event {
            PlayerEvent::Frame { time } => {
                let Some(ts) = &mut self.timeshift else { return };
                ts.time = time;
                self.follow_playhead();
                if let Some(frame) = self.playback_slot.take() {
                    self.display(frame.image);
                }
            }
            // Stay in review at the last frame; going live is always explicit.
            PlayerEvent::CaughtUp => {
                if let Some(ts) = &mut self.timeshift {
                    ts.playing = false;
                }
                self.show_toast("End of buffer. Press L to go live", cx);
            }
            PlayerEvent::Error(err) => {
                self.go_live(cx);
                self.show_toast(err, cx);
            }
        }
        cx.notify();
    }

    fn toggle_play(&mut self, _: &TogglePlay, _: &mut Window, cx: &mut Context<Self>) {
        match &mut self.timeshift {
            Some(ts) if ts.playing => {
                ts.player.pause();
                ts.playing = false;
            }
            Some(ts) => {
                ts.player.play();
                ts.playing = true;
            }
            // Pausing live freezes on the newest frame.
            None => {
                self.enter_timeshift(None, cx);
            }
        }
        cx.notify();
    }

    fn step(&mut self, frames: i64, cx: &mut Context<Self>) {
        if self.timeshift.is_none() && (frames > 0 || !self.enter_timeshift(None, cx)) {
            return;
        }
        if let Some(ts) = &mut self.timeshift {
            ts.player.step(frames);
            ts.playing = false;
        }
        cx.notify();
    }

    fn step_back(&mut self, _: &StepBack, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-self.step_frames, cx);
    }

    fn step_forward(&mut self, _: &StepForward, _: &mut Window, cx: &mut Context<Self>) {
        self.step(self.step_frames, cx);
    }

    fn jump(&mut self, delta: f64, cx: &mut Context<Self>) {
        let Some((_, newest)) = self.buffered() else { return };
        // Live has nothing ahead of it; in review, jumps stop at the buffer's end.
        let Some(from) = self.timeshift.as_ref().map(|ts| ts.time).or((delta < 0.0).then_some(newest))
        else {
            return;
        };
        self.seek(from + delta, cx);
    }

    fn jump_back(&mut self, _: &JumpBack, _: &mut Window, cx: &mut Context<Self>) {
        self.jump(-JUMP_SECS, cx);
    }

    fn jump_forward(&mut self, _: &JumpForward, _: &mut Window, cx: &mut Context<Self>) {
        self.jump(JUMP_SECS, cx);
    }

    fn go_live_action(&mut self, _: &GoLive, _: &mut Window, cx: &mut Context<Self>) {
        self.go_live(cx);
    }

    /// Seeks to the timeline position under `x`. The scale is frozen while
    /// reviewing, so a spot on the bar keeps meaning the same frame; only
    /// LIVE (or `L`) returns to live.
    fn scrub_to(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let (Some(bounds), Some((start, end))) = (self.timeline_bounds.get(), self.timeline_range())
        else {
            return;
        };
        let fraction = ((x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0) as f64;
        self.seek(start + fraction * (end - start), cx);
    }

    /// Update button: download (and verify) an available update, or install a
    /// downloaded one by quitting. Never while recording.
    fn on_update_clicked(&mut self, cx: &mut Context<Self>) {
        if self.recording.is_some() {
            self.show_toast("Finish recording before updating", cx);
            return;
        }
        match self.update.take() {
            Some(Update::Available(release)) => {
                self.update = Some(Update::Downloading(release.clone()));
                let dir = std::env::temp_dir().join("frameback-update");
                let task = cx.background_executor().spawn({
                    let release = release.clone();
                    async move { update::download(&release, &dir) }
                });
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    let _ = this.update(cx, |this, cx| {
                        this.update = Some(match result {
                            Ok(path) => Update::Ready(release, path),
                            Err(err) => {
                                this.show_toast(format!("Update failed: {err:#}"), cx);
                                Update::Available(release)
                            }
                        });
                        cx.notify();
                    });
                })
                .detach();
            }
            Some(Update::Ready(release, path)) => {
                let installed = update::install_path()
                    .ok_or_else(|| anyhow::anyhow!("this build can't update itself"))
                    .and_then(|install| update::install_after_exit(&path, &install, true));
                match installed {
                    // Quitting finalizes the pipeline; the script waits for us to exit.
                    Ok(()) => cx.quit(),
                    Err(err) => {
                        self.show_toast(format!("Update failed: {err:#}"), cx);
                        self.update = Some(Update::Ready(release, path));
                    }
                }
            }
            other => self.update = other,
        }
        cx.notify();
    }

    fn toggle_stats(&mut self, _: &ToggleStats, _: &mut Window, cx: &mut Context<Self>) {
        self.show_stats = !self.show_stats;
        cx.notify();
    }

    fn screenshot(&mut self, _: &Screenshot, _: &mut Window, cx: &mut Context<Self>) {
        let Some(frame) = self.frame.clone() else {
            self.show_toast("No frame to capture yet", cx);
            return;
        };
        let path = output_path(PICTURES_DIR, "frameback", "png");
        let task = cx.background_executor().spawn({
            let path = path.clone();
            async move { save_png(&frame, &path) }
        });
        cx.spawn(async move |this, cx| {
            let message = match task.await {
                Ok(()) => format!("Saved {}", path.display()),
                Err(err) => format!("Screenshot failed: {err:#}"),
            };
            let _ = this.update(cx, |this, cx| this.show_toast(message, cx));
        })
        .detach();
    }
}

const PICTURES_DIR: &str = "Pictures";
const MOVIES_DIR: &str = if cfg!(target_os = "macos") { "Movies" } else { "Videos" };

/// `~/<folder>/<name>-<millis>.<ext>`, falling back to the home directory.
fn output_path(folder: &str, name: &str, extension: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    let preferred = home.join(folder);
    let dir = if preferred.is_dir() { preferred } else { home };
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    dir.join(format!("{name}-{millis}.{extension}"))
}

pub(crate) fn save_png(frame: &RenderImage, path: &std::path::Path) -> anyhow::Result<()> {
    std::fs::write(path, encode_png(frame)?)?;
    Ok(())
}

fn encode_png(frame: &RenderImage) -> anyhow::Result<Vec<u8>> {
    let size = frame.size(0);
    let bgra = frame.as_bytes(0).unwrap_or_default();
    let rgba: Vec<u8> = bgra
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect();
    let mut png = Vec::new();
    image::write_buffer_with_format(
        &mut std::io::Cursor::new(&mut png),
        &rgba,
        size.width.0 as u32,
        size.height.0 as u32,
        image::ExtendedColorType::Rgba8,
        image::ImageFormat::Png,
    )?;
    Ok(png)
}

/// Where copied clips live: they must outlive the paste, so they're files in a
/// temp folder, pruned after a day.
fn copied_clips_dir() -> String {
    let dir = std::env::temp_dir().join("frameback-copied-clips");
    let _ = std::fs::create_dir_all(&dir);
    let day = Duration::from_secs(24 * 60 * 60);
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if old.is_some_and(|age| age > day) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    dir.to_string_lossy().into_owned()
}

// --- Rendering -------------------------------------------------------------

fn panel_bg() -> Hsla {
    rgba(0x18181be6).into()
}
fn border() -> Hsla {
    rgba(0xffffff1a).into()
}
fn text() -> Hsla {
    rgb(0xe4e4e7).into()
}
fn muted_text() -> Hsla {
    rgb(0xa1a1aa).into()
}
fn hover_bg() -> Hsla {
    rgba(0xffffff14).into()
}
fn review_amber() -> Hsla {
    rgb(0xf59e0b).into()
}
fn recording_red() -> Hsla {
    rgb(0xef4444).into()
}
fn accent() -> Hsla {
    rgb(0x22c55e).into()
}

impl Render for Frameback {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.to_free.drain(..) {
            let _ = window.drop_image(image);
        }
        self.to_free = std::mem::take(&mut self.retired);

        let hide_cursor = !self.controls_visible && self.menu.is_none();

        div()
            .id("root")
            .track_focus(&self.focus)
            .key_context("Frameback")
            .on_action(cx.listener(Self::toggle_fullscreen))
            .on_action(cx.listener(Self::exit_fullscreen))
            .on_action(cx.listener(Self::toggle_mute))
            .on_action(cx.listener(Self::toggle_stats))
            .on_action(cx.listener(Self::screenshot))
            .on_action(cx.listener(Self::toggle_record))
            .on_action(cx.listener(Self::toggle_play))
            .on_action(cx.listener(Self::step_back))
            .on_action(cx.listener(Self::step_forward))
            .on_action(cx.listener(Self::jump_back))
            .on_action(cx.listener(Self::jump_forward))
            .on_action(cx.listener(Self::go_live_action))
            .on_action(cx.listener(|this, _: &ToggleClipMode, _, cx| this.toggle_clip_mode(cx)))
            .on_action(cx.listener(|this, _: &MarkStart, _, cx| this.mark(true, cx)))
            .on_action(cx.listener(|this, _: &MarkEnd, _, cx| this.mark(false, cx)))
            .on_action(cx.listener(|this, _: &SaveClip, _, cx| this.save_clip(cx)))
            .on_action(cx.listener(|this, _: &CopyClip, _, cx| this.copy_clip(cx)))
            .on_action(cx.listener(|this, _: &CopyFrame, _, cx| this.copy_frame(cx)))
            .on_mouse_move(cx.listener(|this, _, _, cx| this.reveal_controls(cx)))
            .relative()
            .size_full()
            .bg(gpui::black())
            .text_color(text())
            .text_sm()
            .when(hide_cursor, |d| d.cursor(CursorStyle::None))
            .child(self.render_video(cx))
            .when(self.timeshift.is_some(), |d| {
                // An unmistakable frame around the picture while reviewing.
                let color = if self.editing_clip() { accent() } else { review_amber() };
                d.child(div().absolute().inset_0().border_2().border_color(color))
            })
            .children(self.render_mode_badge())
            .when(self.show_stats, |d| d.child(self.render_stats()))
            .when_some(self.toast.clone(), |d, toast| d.child(render_toast(toast)))
            .when(self.menu.is_some(), |d| {
                d.child(
                    deferred(
                        div()
                            .id("menu-backdrop")
                            .absolute()
                            .inset_0()
                            .occlude()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.menu = None;
                                    cx.notify();
                                }),
                            ),
                    )
                    .with_priority(1),
                )
            })
            .when(self.controls_visible || self.menu.is_some() || self.timeshift.is_some(), |d| {
                d.child(self.render_toolbar(cx))
            })
    }
}

impl Frameback {
    fn render_video(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.frame {
            Some(frame) => img(frame.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            None => div()
                .text_color(muted_text())
                .child(if self.video_devices.is_empty() {
                    "Waiting for a capture device…"
                } else {
                    "Starting capture…"
                })
                .into_any_element(),
        };
        div()
            .id("video")
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|_, event: &ClickEvent, window, _| {
                if event.click_count() == 2 {
                    window.toggle_fullscreen();
                }
            }))
            .child(content)
    }

    fn render_stats(&self) -> impl IntoElement {
        let format = self.active_format.as_ref().map(VideoFormat::label).unwrap_or_default();
        let latency = match self.stats.latency_ms {
            Some(ms) => format!("capture→UI {ms:.1} ms"),
            None => "capture→UI n/a".into(),
        };
        let audio = match &self.monitor {
            Some(m) => format!("audio {} Hz", m.sample_rate),
            None => "audio off".into(),
        };
        let encoder = self.dvr.as_ref().map(|d| {
            format!(
                "{} · {} dropped",
                d.encoder.as_deref().unwrap_or("starting encoder"),
                self.slot.tap_dropped()
            )
        });
        let buffer = self.dvr.as_ref().and_then(|d| d.store.as_ref()).map(|store| {
            let span = store.video_range().map_or(0.0, |(a, b)| b - a);
            format!("dvr {} · {}", format_time(span), format_bytes(store.bytes()))
        });
        div()
            .absolute()
            .top_3()
            .left_3()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(panel_bg())
            .text_xs()
            .font_family("monospace")
            .flex()
            .flex_col()
            .child(format!("Frameback v{}", update::VERSION))
            .child(format)
            .child(format!("{:.1} fps", self.stats.fps))
            .child(latency)
            .child(audio)
            .children(encoder)
            .children(buffer)
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let video_label = self
            .video_selected
            .map(|i| self.video_devices[i].name.clone())
            .unwrap_or_else(|| "No device".into());
        let format_label = self
            .active_format
            .as_ref()
            .map(VideoFormat::label)
            .unwrap_or_else(|| "Format".into());
        let audio_label = self
            .audio_selected
            .map(|i| self.audio_devices[i].name.clone())
            .unwrap_or_else(|| "Audio off".into());
        let muted = self.muted_volume.is_some();
        let record_label = match &self.recording {
            Some(r) => {
                let secs = r.started.elapsed().as_secs();
                format!("■ {:02}:{:02}", secs / 60, secs % 60)
            }
            None => "● Rec".into(),
        };

        let dvr_label = match self.dvr.as_ref().and_then(|d| d.store.as_ref()) {
            Some(store) => format!("DVR {} min · {}", self.dvr_window_min, format_bytes(store.bytes())),
            None => format!("DVR {} min", self.dvr_window_min),
        };

        div()
            .id("controls")
            .absolute()
            .bottom_4()
            .left_4()
            .right_4()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .occlude()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.toolbar_hovered = *hovered;
                if !hovered {
                    this.reveal_controls(cx);
                }
            }))
            .children(self.render_timeline(cx))
            .child(
                div()
                    .id("toolbar")
                    .flex()
                    .items_center()
                    .gap_1()
                    .p_1()
                    .rounded_lg()
                    .bg(panel_bg())
                    .border_1()
                    .border_color(border())
                    .shadow_lg()
                    .child(
                        button("record", record_label)
                            .text_color(muted_text())
                            .when(self.recording.is_some(), |b| {
                                b.text_color(recording_red()).bg(rgba(0xef44441f))
                            })
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.dispatch_action(Box::new(ToggleRecord), cx)
                            })),
                    )
                    .child(self.menu_button(Menu::Codec, "codec", self.codec.label(), cx))
                    .child(self.menu_button(Menu::Dvr, "dvr", dvr_label, cx))
                    .child(separator())
                    .child(self.menu_button(Menu::Video, "video", video_label, cx))
                    .child(self.menu_button(Menu::Format, "format", format_label, cx))
                    .child(separator())
                    .child(self.menu_button(Menu::Audio, "audio", audio_label, cx))
                    .child(
                        button("mute", if muted { "Unmute" } else { "Mute" })
                            .when(muted, |b| b.text_color(muted_text()))
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.dispatch_action(Box::new(ToggleMute), cx)
                            })),
                    )
                    .child(separator())
                    .child(
                        button("stats", "Stats")
                            .when(self.show_stats, |b| b.text_color(accent()))
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.dispatch_action(Box::new(ToggleStats), cx)
                            })),
                    )
                    .child(button("shot", "Screenshot").on_click(cx.listener(
                        |_, _, window, cx| window.dispatch_action(Box::new(Screenshot), cx),
                    )))
                    .child(button("full", "Fullscreen").on_click(cx.listener(
                        |_, _, window, _| window.toggle_fullscreen(),
                    )))
                    .children(self.update.as_ref().map(|update| {
                        let (label, ready) = match update {
                            Update::Available(r) => (format!("Update to v{}", r.version), true),
                            Update::Downloading(r) => (format!("Downloading v{}…", r.version), false),
                            Update::Ready(..) => ("Restart to update".to_string(), true),
                        };
                        button("update", label)
                            .when(ready, |b| b.bg(accent()).text_color(gpui::black()))
                            .when(!ready, |b| b.text_color(muted_text()))
                            .on_click(cx.listener(|this, _, _, cx| this.on_update_clicked(cx)))
                    })),
            )
    }

    /// LIVE or REVIEW, top centre. Review is always shown; live only with the controls.
    fn render_mode_badge(&self) -> Option<impl IntoElement> {
        let review = self.timeshift.is_some();
        if !review && !(self.controls_visible && self.dvr.is_some()) {
            return None;
        }
        let (color, mode, detail) = if self.editing_clip() {
            (accent(), "CLIP", "buffer paused · Esc to close")
        } else if review {
            (review_amber(), "REVIEW", "buffer paused · L to go live")
        } else {
            (recording_red(), "● LIVE", "buffering")
        };
        Some(
            div().absolute().top_3().left_0().right_0().flex().justify_center().child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl_1()
                    .pr_3()
                    .py_1()
                    .rounded_full()
                    .bg(panel_bg())
                    .border_1()
                    .border_color(color)
                    .text_xs()
                    .child(
                        div()
                            .px_2()
                            .rounded_full()
                            .bg(color)
                            .text_color(gpui::black())
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(mode),
                    )
                    .child(div().text_color(muted_text()).child(detail)),
            ),
        )
    }

    fn render_timeline(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (start, end) = self.timeline_range()?;
        let span = (end - start).max(1e-3);
        let live = self.timeshift.is_none();
        let position = self.timeshift.as_ref().map_or(end, |ts| ts.time);
        let fraction = ((position - start) / span).clamp(0.0, 1.0) as f32;
        let playing = self.timeshift.as_ref().is_some_and(|ts| ts.playing);
        let bounds = self.timeline_bounds.clone();

        let position_label = if live {
            format!("{} buffered", format_time(span))
        } else {
            format!("{} / {}", format_time(position - start), format_time(span))
        };
        // Marked clip, as fractions of the track.
        let to_fraction = |t: f64| ((t - start) / span).clamp(0.0, 1.0) as f32;
        let clip = self
            .timeshift
            .as_ref()
            .filter(|ts| ts.editing_clip)
            .map(|ts| (ts.clip_start, ts.clip_end));
        let selection = self.clip_range().filter(|_| {
            clip.is_some_and(|(a, b)| a.is_some() || b.is_some())
        });
        let markers: Vec<f32> = clip
            .into_iter()
            .flat_map(|(a, b)| [a, b])
            .flatten()
            .map(to_fraction)
            .collect();
        let n = self.step_frames;
        let step_label = format!("{n} frame{}", if n == 1 { "" } else { "s" });
        // While dragging, follow the mouse anywhere in the window: the controls
        // panel occludes the root, so element-level move events wouldn't arrive.
        let view = cx.entity();
        let scrubbing = self.scrubbing;
        let follow_drag = move |_: Bounds<Pixels>, _: (), window: &mut Window, _: &mut App| {
            if !scrubbing {
                return;
            }
            let moving = view.clone();
            window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, _, cx| {
                if phase == gpui::DispatchPhase::Bubble && e.pressed_button == Some(MouseButton::Left) {
                    moving.update(cx, |this, cx| this.scrub_to(e.position.x, cx));
                }
            });
            window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, _, cx| {
                if phase == gpui::DispatchPhase::Bubble {
                    view.update(cx, |this, cx| {
                        this.scrubbing = false;
                        cx.notify();
                    });
                }
            });
        };
        let track = div()
            .id("timeline")
            .flex_1()
            .h(px(20.))
            .flex()
            .items_center()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.scrubbing = true;
                    this.scrub_to(event.position.x, cx);
                }),
            )
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgba(0xffffff26))
                    .child(
                        canvas(move |b, _, _| bounds.set(Some(b)), follow_drag)
                            .absolute()
                            .size_full(),
                    )
                    .children(selection.map(|(a, b)| {
                        div()
                            .absolute()
                            .top(px(-3.))
                            .h(px(10.))
                            .left(gpui::relative(to_fraction(a)))
                            .w(gpui::relative(to_fraction(b) - to_fraction(a)))
                            .bg(rgba(0x22c55e59))
                    }))
                    .children(markers.into_iter().map(|f| {
                        div()
                            .absolute()
                            .top(px(-6.))
                            .h(px(16.))
                            .w(px(2.))
                            .left(gpui::relative(f))
                            .bg(accent())
                    }))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .h_full()
                            .w(gpui::relative(fraction))
                            .rounded_full()
                            .bg(if live { recording_red() } else { review_amber() }),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(-4.))
                            .left(gpui::relative(fraction))
                            .ml(px(-6.))
                            .size(px(12.))
                            .rounded_full()
                            .bg(gpui::white()),
                    ),
            );

        Some(
            div()
                .w_full()
                .max_w(px(960.))
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_lg()
                .bg(panel_bg())
                .border_1()
                .border_color(border())
                .text_xs()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            button("play", if playing || live { "❚❚" } else { "▶" })
                                .w(px(28.))
                                .justify_center()
                                .on_click(cx.listener(|_, _, window, cx| {
                                    window.dispatch_action(Box::new(TogglePlay), cx)
                                })),
                        )
                        .child(
                            div()
                                .min_w(px(96.))
                                .font_family("monospace")
                                .text_color(muted_text())
                                .child(position_label),
                        )
                        .child(track)
                        .when(!live, |row| {
                            row.child(button("step-back", "◀").on_click(cx.listener(
                                |_, _, window, cx| window.dispatch_action(Box::new(StepBack), cx),
                            )))
                            .child(self.menu_button(Menu::Step, "step", step_label, cx))
                            .child(button("step-forward", "▶").on_click(cx.listener(
                                |_, _, window, cx| window.dispatch_action(Box::new(StepForward), cx),
                            )))
                        })
                        .child(separator())
                        .child(
                            button("clip", "Clip")
                                .when(self.editing_clip(), |b| {
                                    b.bg(accent()).text_color(gpui::black())
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_clip_mode(cx))),
                        )
                        .child(if live {
                            // A status, not a control: nothing to click while live.
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .px_2()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(recording_red())
                                .child(div().size(px(6.)).rounded_full().bg(recording_red()))
                                .child("LIVE")
                                .into_any_element()
                        } else {
                            button("live", "Go live")
                                .on_click(cx.listener(|this, _, _, cx| this.go_live(cx)))
                                .into_any_element()
                        }),
                )
                .children(self.editing_clip().then(|| self.render_clip_row(cx))),
        )
    }

    /// Start/end marks and Save clip, shown while reviewing.
    fn render_clip_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ts = self.timeshift.as_ref();
        let (start, end) = (ts.and_then(|t| t.clip_start), ts.and_then(|t| t.clip_end));
        let origin = self.timeline_range().map_or(0.0, |r| r.0);
        let mark = |t: Option<f64>| t.map_or("—".to_string(), |t| format_precise(t - origin));
        let summary = self.clip_range().map(|(a, b)| {
            let frames = self
                .dvr
                .as_ref()
                .and_then(|d| d.store.as_ref())
                .map_or(0, |store| store.frames_between(a, b));
            let whole = start.is_none() && end.is_none();
            format!(
                "{}{} · {frames} frames",
                if whole { "whole buffer · " } else { "" },
                format_precise(b - a)
            )
        });
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().text_color(muted_text()).child("Clip"))
            .child(button("mark-start", format!("[ Start {}", mark(start))).on_click(
                cx.listener(|this, _, _, cx| this.mark(true, cx)),
            ))
            .child(button("mark-end", format!("End {} ]", mark(end))).on_click(
                cx.listener(|this, _, _, cx| this.mark(false, cx)),
            ))
            .children((start.is_some() || end.is_some()).then(|| {
                button("clear-marks", "✕").text_color(muted_text()).on_click(cx.listener(
                    |this, _, _, cx| {
                        if let Some(ts) = &mut this.timeshift {
                            ts.clip_start = None;
                            ts.clip_end = None;
                        }
                        cx.notify();
                    },
                ))
            }))
            .child(div().flex_1().font_family("monospace").text_color(muted_text()).children(summary))
            .child(button("copy-frame", "Copy frame").on_click(cx.listener(|this, _, _, cx| this.copy_frame(cx))))
            .child(button("copy-clip", "Copy clip").on_click(cx.listener(|this, _, _, cx| this.copy_clip(cx))))
            .child(
                button("save-clip", "Save clip")
                    .bg(accent())
                    .text_color(gpui::black())
                    .font_weight(gpui::FontWeight::BOLD)
                    .on_click(cx.listener(|this, _, _, cx| this.save_clip(cx))),
            )
    }

    fn menu_button(
        &self,
        menu: Menu,
        id: &'static str,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.menu == Some(menu);
        div()
            .relative()
            .child(
                button(id, label)
                    .max_w(px(220.))
                    .overflow_hidden()
                    .when(open, |b| b.bg(hover_bg()))
                    .child(div().text_color(muted_text()).child("▾"))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_menu(menu, cx))),
            )
            .when(open, |d| {
                d.child(deferred(self.render_menu(menu, cx)).with_priority(2))
            })
    }

    fn render_menu(&self, menu: Menu, cx: &mut Context<Self>) -> impl IntoElement {
        let items: Vec<(SharedString, bool, Box<dyn Fn(&mut Self, &mut Context<Self>)>)> =
            match menu {
                Menu::Video => self
                    .video_devices
                    .iter()
                    .enumerate()
                    .map(|(i, d)| {
                        (
                            d.name.clone().into(),
                            self.video_selected == Some(i),
                            Box::new(move |this: &mut Self, cx: &mut Context<Self>| {
                                this.select_video(i, cx)
                            }) as Box<dyn Fn(&mut Self, &mut Context<Self>)>,
                        )
                    })
                    .collect(),
                Menu::Format => self
                    .formats
                    .iter()
                    .map(|f| {
                        let f = f.clone();
                        (
                            f.label().into(),
                            self.active_format.as_ref() == Some(&f),
                            Box::new(move |this: &mut Self, cx: &mut Context<Self>| {
                                this.select_format(f.clone(), cx)
                            }) as Box<dyn Fn(&mut Self, &mut Context<Self>)>,
                        )
                    })
                    .collect(),
                Menu::Codec => Codec::ALL
                    .iter()
                    .map(|&codec| {
                        (
                            format!("{} (.{})", codec.label(), codec.extension()).into(),
                            self.codec == codec,
                            Box::new(move |this: &mut Self, cx: &mut Context<Self>| {
                                this.select_codec(codec, cx)
                            }) as Box<dyn Fn(&mut Self, &mut Context<Self>)>,
                        )
                    })
                    .collect(),
                Menu::Step => STEP_SIZES
                    .iter()
                    .map(|&n| {
                        (
                            format!("Step {n} frame{}", if n == 1 { "" } else { "s" }).into(),
                            self.step_frames == n,
                            Box::new(move |this: &mut Self, cx: &mut Context<Self>| {
                                this.step_frames = n;
                                cx.notify();
                            }) as Box<dyn Fn(&mut Self, &mut Context<Self>)>,
                        )
                    })
                    .collect(),
                Menu::Dvr => DVR_WINDOWS_MIN
                    .iter()
                    .map(|&minutes| {
                        (
                            format!("Keep the last {minutes} min").into(),
                            self.dvr_window_min == minutes,
                            Box::new(move |this: &mut Self, cx: &mut Context<Self>| {
                                this.select_dvr_window(minutes, cx)
                            }) as Box<dyn Fn(&mut Self, &mut Context<Self>)>,
                        )
                    })
                    .collect(),
                Menu::Audio => std::iter::once((
                    SharedString::from("Off"),
                    self.audio_selected.is_none(),
                    Box::new(|this: &mut Self, cx: &mut Context<Self>| this.select_audio(None, cx))
                        as Box<dyn Fn(&mut Self, &mut Context<Self>)>,
                ))
                .chain(self.audio_devices.iter().enumerate().map(|(i, d)| {
                    (
                        d.name.clone().into(),
                        self.audio_selected == Some(i),
                        Box::new(move |this: &mut Self, cx: &mut Context<Self>| {
                            this.select_audio(Some(i), cx)
                        }) as Box<dyn Fn(&mut Self, &mut Context<Self>)>,
                    )
                }))
                .collect(),
            };

        div()
            .id("menu")
            .occlude()
            .absolute()
            .bottom(px(40.))
            .left_0()
            .min_w(px(220.))
            .max_h(px(360.))
            .overflow_y_scroll()
            .p_1()
            .rounded_lg()
            .bg(panel_bg())
            .border_1()
            .border_color(border())
            .shadow_lg()
            .flex()
            .flex_col()
            .when(items.is_empty(), |d| {
                d.child(div().px_2().py_1().text_color(muted_text()).child("Nothing available"))
            })
            .children(items.into_iter().enumerate().map(|(i, (label, selected, action))| {
                div()
                    .id(("item", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(hover_bg()))
                    .child(
                        div()
                            .w(px(12.))
                            .text_color(accent())
                            .child(if selected { "●" } else { "" }),
                    )
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = None;
                        action(this, cx);
                    }))
            }))
    }
}

fn button(id: &'static str, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .whitespace_nowrap()
        .hover(|s| s.bg(hover_bg()))
        .child(label.into())
}

#[cfg(test)]
mod tests {
    use super::visible_range;

    #[test]
    fn frozen_timeline_ignores_new_footage() {
        // Reviewing a buffer that spanned 0–100 s; capture has since reached 130 s.
        assert_eq!(visible_range(Some((0.0, 100.0)), (0.0, 130.0)), (0.0, 100.0));
        // Live, the timeline is the whole buffer.
        assert_eq!(visible_range(None, (0.0, 130.0)), (0.0, 130.0));
    }

    #[test]
    fn frozen_timeline_drops_pruned_footage() {
        // The oldest 20 s aged out of the rewind window while reviewing.
        assert_eq!(visible_range(Some((0.0, 100.0)), (20.0, 160.0)), (20.0, 100.0));
    }
}

fn format_time(secs: f64) -> String {
    let secs = secs.max(0.0).round() as u64;
    match secs / 3600 {
        0 => format!("{}:{:02}", secs / 60, secs % 60),
        h => format!("{h}:{:02}:{:02}", secs / 60 % 60, secs % 60),
    }
}

/// `m:ss.cc`, precise enough to tell neighbouring frames apart.
fn format_precise(secs: f64) -> String {
    let centis = (secs.max(0.0) * 100.0).round() as u64;
    format!("{}:{:02}.{:02}", centis / 6000, centis / 100 % 60, centis % 100)
}

fn format_bytes(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b => format!("{} MB", b >> 20),
    }
}

fn separator() -> impl IntoElement {
    div().w(px(1.)).h(px(16.)).mx_1().bg(border())
}

fn render_toast(message: SharedString) -> impl IntoElement {
    div()
        .absolute()
        // Below the mode badge.
        .top(px(48.))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .px_3()
                .py_1p5()
                .rounded_lg()
                .bg(panel_bg())
                .border_1()
                .border_color(border())
                .child(message),
        )
}

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("f", ToggleFullscreen, None),
        KeyBinding::new("escape", ExitFullscreen, None),
        KeyBinding::new("s", Screenshot, None),
        KeyBinding::new("m", ToggleMute, None),
        KeyBinding::new("i", ToggleStats, None),
        KeyBinding::new("r", ToggleRecord, None),
        KeyBinding::new("space", TogglePlay, None),
        KeyBinding::new("left", StepBack, None),
        KeyBinding::new("right", StepForward, None),
        KeyBinding::new("shift-left", JumpBack, None),
        KeyBinding::new("shift-right", JumpForward, None),
        KeyBinding::new("l", GoLive, None),
        KeyBinding::new("end", GoLive, None),
        KeyBinding::new("c", ToggleClipMode, None),
        KeyBinding::new("[", MarkStart, None),
        KeyBinding::new("]", MarkEnd, None),
        KeyBinding::new("secondary-s", SaveClip, None),
        KeyBinding::new("secondary-c", CopyClip, None),
        KeyBinding::new("secondary-shift-c", CopyFrame, None),
        KeyBinding::new("secondary-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}
