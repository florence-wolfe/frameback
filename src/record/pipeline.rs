//! The recording pipeline: one encoder per stream, always running while the
//! capture is, feeding the DVR buffer and (while recording) a file.
//!
//! The live preview never waits on this thread: frames and audio arrive
//! through bounded channels the capture side only `try_send`s into.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg::codec::{self, threading};
use ffmpeg::format::{Pixel, Sample, sample};
use ffmpeg::software::scaling;
use ffmpeg::{ChannelLayout, Dictionary, Packet, Rational, color, encoder, frame};
use futures::channel::mpsc::UnboundedSender;

use super::codec::{Codec, time_base};
use super::muxer::Muxer;
use super::store::{OwnedParameters, Store, StoreReader, StoreWriter, StreamDesc, StreamKind};
use crate::audio::{AudioChunk, RECORD_CHANNELS};
use crate::video::{TapFrame, VideoFormat};

/// Frames buffered between capture and encoder (~0.25 s at 60 fps). Beyond
/// this, frames are dropped from the recording rather than delaying capture.
const QUEUE_FRAMES: usize = 16;
/// Audio chunks buffered (~1 s at 5 ms callbacks).
const QUEUE_AUDIO: usize = 200;

pub enum PipelineEvent {
    Ready { encoder: String, store: Store },
    Recording { path: PathBuf },
    Saved { path: PathBuf, frames: u64 },
    /// A recording failed; the pipeline and DVR keep running.
    Error(String),
    /// The pipeline stopped (e.g. no encoder, disk full).
    Failed(String),
}

enum Command {
    /// Start a file at the keyframe at or before this capture-clock time.
    Record(PathBuf, Instant),
    StopRecording,
    /// Stop (true) or resume (false) adding to the DVR buffer.
    SetFrozen(bool),
}

/// Senders for the capture side to `try_send` into.
pub struct Taps {
    pub video: SyncSender<TapFrame>,
    pub audio: Option<SyncSender<AudioChunk>>,
}

/// Runs until its video tap is dropped; then finalizes any recording.
pub struct Pipeline {
    commands: mpsc::Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Pipeline {
    pub fn start(
        codec: Codec,
        format: VideoFormat,
        audio_rate: Option<u32>,
        window: Duration,
        events: UnboundedSender<PipelineEvent>,
    ) -> (Pipeline, Taps) {
        let (video_tx, video_rx) = sync_channel(QUEUE_FRAMES);
        let (audio_tx, audio_rx) = sync_channel(QUEUE_AUDIO);
        let (commands, command_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("frameback-pipeline".into())
            .spawn(move || {
                let inputs = Inputs { video: video_rx, audio: audio_rx, commands: command_rx };
                if let Err(err) = run(codec, &format, audio_rate, window, inputs, &events) {
                    let _ = events.unbounded_send(PipelineEvent::Failed(format!("{err:#}")));
                }
            })
            .expect("spawn pipeline thread");
        let taps = Taps { video: video_tx, audio: audio_rate.map(|_| audio_tx) };
        (Pipeline { commands, thread: Some(thread) }, taps)
    }

    /// Records from `from` (normally when the user pressed record): the file
    /// starts at the keyframe at or before it, using what's already buffered.
    pub fn record(&self, path: PathBuf, from: Instant) {
        let _ = self.commands.send(Command::Record(path, from));
    }

    /// While frozen, nothing is added to (or aged out of) the DVR buffer, so
    /// it can be reviewed without being overwritten. Recording continues.
    pub fn set_frozen(&self, frozen: bool) {
        let _ = self.commands.send(Command::SetFrozen(frozen));
    }

    pub fn stop_recording(&self) {
        let _ = self.commands.send(Command::StopRecording);
    }

    /// Waits for the thread to finish. Its taps must already be dropped.
    pub fn join(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Inputs {
    video: Receiver<TapFrame>,
    audio: Receiver<AudioChunk>,
    commands: Receiver<Command>,
}

/// Snaps capture times onto a constant-frame-rate grid.
pub struct FrameClock {
    start: Option<Instant>,
    period: Duration,
    last: Option<i64>,
}

impl FrameClock {
    pub fn new(time_base: Rational) -> Self {
        let period = Duration::from_secs_f64(time_base.0 as f64 / time_base.1 as f64);
        Self { start: None, period, last: None }
    }

    /// The session origin: the first frame's capture time.
    pub fn origin(&self) -> Option<Instant> {
        self.start
    }

    /// Frame index for a capture time, or None if that slot is already taken.
    /// Gaps simply skip indices; players hold the previous frame.
    pub fn index(&mut self, at: Instant) -> Option<i64> {
        let start = *self.start.get_or_insert(at);
        let elapsed = at.saturating_duration_since(start).as_secs_f64();
        let index = (elapsed / self.period.as_secs_f64()).round() as i64;
        if self.last.is_some_and(|last| index <= last) {
            return None;
        }
        self.last = Some(index);
        Some(index)
    }
}

/// Where a captured audio chunk goes on the timeline.
#[derive(Debug, PartialEq)]
pub struct Placement {
    /// Sample index of the chunk's first sample (or of the inserted silence).
    pub pts: i64,
    /// Leading samples to drop (audio ran ahead of the video clock).
    pub skip: usize,
    /// Silence to insert first (audio fell behind).
    pub pad: usize,
}

/// Keeps audio sample-continuous, but resyncs to capture timestamps when the
/// device's sample clock drifts more than `tolerance` samples from the video's.
pub struct AudioClock {
    rate: f64,
    tolerance: i64,
    next: Option<i64>,
}

impl AudioClock {
    pub fn new(rate: u32, tolerance: i64) -> Self {
        Self { rate: rate as f64, tolerance, next: None }
    }

    /// `offset` is the chunk's capture time relative to the session origin.
    pub fn place(&mut self, offset: f64, frames: usize) -> Placement {
        let expected = (offset * self.rate).round() as i64;
        let placement = match self.next {
            None if expected < 0 => Placement { pts: 0, skip: (-expected) as usize, pad: 0 },
            None => Placement { pts: expected, skip: 0, pad: 0 },
            Some(next) => {
                let drift = expected - next;
                if drift.abs() <= self.tolerance {
                    Placement { pts: next, skip: 0, pad: 0 }
                } else if drift > 0 {
                    Placement { pts: next, skip: 0, pad: drift as usize }
                } else {
                    Placement { pts: next, skip: (-drift) as usize, pad: 0 }
                }
            }
        };
        let skip = placement.skip.min(frames);
        self.next = Some(placement.pts + placement.pad as i64 + (frames - skip) as i64);
        Placement { skip, ..placement }
    }
}

struct VideoEncoder {
    encoder: encoder::video::Encoder,
    scaler: scaling::Context,
    bgra: frame::Video,
    converted: frame::Video,
    clock: FrameClock,
    width: u32,
    height: u32,
}

struct AudioEncoder {
    encoder: encoder::audio::Encoder,
    sample: Sample,
    frame_size: usize,
    clock: AudioClock,
    /// Interleaved stereo samples not yet encoded, starting at `fifo_pts`.
    fifo: Vec<f32>,
    fifo_pts: Option<i64>,
    rate: u32,
}

struct Sinks {
    codec: Codec,
    store: StoreWriter,
    reader: StoreReader,
    recording: Option<Muxer>,
    events: UnboundedSender<PipelineEvent>,
    buffering: Buffering,
}

#[derive(Clone, Copy, PartialEq)]
enum Buffering {
    On,
    Frozen,
    /// Unfrozen, but skipping packets until a video keyframe so the buffer
    /// never holds frames whose reference frames are missing.
    AwaitingKeyframe,
}

impl Sinks {
    fn emit(&mut self, stream: usize, packet: &Packet) -> Result<()> {
        let Some(data) = packet.data() else { return Ok(()) };
        let pts = packet.pts().unwrap_or(0);
        let duration = packet.duration().max(1);
        let key = packet.is_key();
        let video_key = self.store.store().streams()[stream].kind == StreamKind::Video && key;
        if self.buffering == Buffering::AwaitingKeyframe && video_key {
            self.buffering = Buffering::On;
        }
        let buffered = self.buffering == Buffering::On;
        if buffered {
            self.store.append(stream, pts, duration, key, data)?;
        }
        let Some(muxer) = &mut self.recording else { return Ok(()) };
        let streams = self.store.store().streams();
        let result = if muxer.started() {
            muxer.write(streams, stream, pts, duration, key, data)
        } else if video_key && buffered {
            // Nothing was buffered at the record point; start here (this
            // packet is in the store now, so the backfill writes it).
            self.begin_recording(streams[stream].seconds(pts))
        } else if video_key {
            muxer.set_start(streams[stream].seconds(pts));
            muxer.write(streams, stream, pts, duration, key, data)
        } else {
            Ok(())
        };
        if let Err(err) = result {
            self.recording = None;
            let _ = self.events.unbounded_send(PipelineEvent::Error(format!("{err:#}")));
        }
        Ok(())
    }

    /// Starts the file at `start` and writes everything already buffered from there.
    fn begin_recording(&mut self, start: f64) -> Result<()> {
        let store = self.store.store().clone();
        let Some(muxer) = &mut self.recording else { return Ok(()) };
        muxer.set_start(start);
        for (stream, desc) in store.streams().iter().enumerate() {
            for entry in store.range(stream, desc.pts(start), i64::MAX) {
                let data = self.reader.read(&entry)?;
                muxer.write(store.streams(), stream, entry.pts, entry.duration, entry.key, &data)?;
            }
        }
        Ok(())
    }

    fn command(&mut self, command: Command, origin: Option<Instant>) {
        match command {
            Command::Record(path, from) => {
                self.finish_recording();
                let muxer = match Muxer::create(path.clone(), self.codec, self.store.store().streams(), true) {
                    Ok(muxer) => muxer,
                    Err(err) => {
                        let _ = self.events.unbounded_send(PipelineEvent::Error(format!("{err:#}")));
                        return;
                    }
                };
                self.recording = Some(muxer);
                let _ = self.events.unbounded_send(PipelineEvent::Recording { path });
                // Audio (and queued video) for the record point may already be
                // encoded, so start from the buffer rather than the next packet.
                let store = self.store.store().clone();
                let start = origin.zip(store.stream_of(StreamKind::Video)).and_then(|(origin, video)| {
                    let desc = &store.streams()[video];
                    let at = from.saturating_duration_since(origin).as_secs_f64();
                    store.keyframe_at_or_before(video, desc.pts(at)).map(|e| desc.seconds(e.pts))
                });
                // A frozen buffer holds older footage, not the record point.
                if let Some(start) = start.filter(|_| self.buffering == Buffering::On)
                    && let Err(err) = self.begin_recording(start)
                {
                    self.recording = None;
                    let _ = self.events.unbounded_send(PipelineEvent::Error(format!("{err:#}")));
                }
            }
            Command::StopRecording => self.finish_recording(),
            Command::SetFrozen(true) => self.buffering = Buffering::Frozen,
            Command::SetFrozen(false) => {
                if self.buffering == Buffering::Frozen {
                    self.buffering = Buffering::AwaitingKeyframe;
                }
            }
        }
    }

    fn finish_recording(&mut self) {
        let Some(muxer) = self.recording.take() else { return };
        let event = match muxer.finish() {
            Ok((path, frames)) => PipelineEvent::Saved { path, frames },
            Err(err) => PipelineEvent::Error(format!("{err:#}")),
        };
        let _ = self.events.unbounded_send(event);
    }
}

fn run(
    codec: Codec,
    format: &VideoFormat,
    audio_rate: Option<u32>,
    window: Duration,
    inputs: Inputs,
    events: &UnboundedSender<PipelineEvent>,
) -> Result<()> {
    ffmpeg::init().context("initializing FFmpeg")?;
    let video_tb = time_base(format.fps);
    let (mut video, encoder_name) = open_video(codec, format, video_tb)?;
    let mut audio = audio_rate.map(|rate| open_audio(codec, rate, video_tb)).transpose()?;

    let mut streams = vec![StreamDesc {
        kind: StreamKind::Video,
        time_base: video_tb,
        params: OwnedParameters::new(codec::Parameters::from(&video.encoder)),
    }];
    if let Some(audio) = &audio {
        streams.push(StreamDesc {
            kind: StreamKind::Audio,
            time_base: Rational(1, audio.rate as i32),
            params: OwnedParameters::new(codec::Parameters::from(&audio.encoder)),
        });
    }
    let store = Store::create(streams, window)?;
    let _ = events.unbounded_send(PipelineEvent::Ready {
        encoder: encoder_name,
        store: store.store().clone(),
    });
    let reader = store.store().reader();
    let mut sinks = Sinks {
        codec,
        store,
        reader,
        recording: None,
        events: events.clone(),
        buffering: Buffering::On,
    };

    loop {
        match inputs.video.recv_timeout(Duration::from_millis(50)) {
            Ok(tap) => encode_video(&mut video, tap, &mut sinks)?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(audio) = &mut audio {
            while let Ok(chunk) = inputs.audio.try_recv() {
                encode_audio(audio, chunk, video.clock.origin(), &mut sinks)?;
            }
        }
        while let Ok(command) = inputs.commands.try_recv() {
            sinks.command(command, video.clock.origin());
        }
    }

    video.encoder.send_eof()?;
    drain(&mut video.encoder, 0, &mut sinks)?;
    if let Some(audio) = &mut audio {
        audio.encoder.send_eof()?;
        drain(&mut audio.encoder, 1, &mut sinks)?;
    }
    sinks.finish_recording();
    Ok(())
}

fn encode_video(video: &mut VideoEncoder, tap: TapFrame, sinks: &mut Sinks) -> Result<()> {
    let size = tap.image.size(0);
    if (size.width.0 as u32, size.height.0 as u32) != (video.width, video.height) {
        return Ok(());
    }
    let Some(index) = video.clock.index(tap.at) else { return Ok(()) };
    copy_into(tap.image.as_bytes(0).unwrap_or_default(), video.width, &mut video.bgra);
    video.scaler.run(&video.bgra, &mut video.converted)?;
    video.converted.set_pts(Some(index));
    video.encoder.send_frame(&video.converted)?;
    drain(&mut video.encoder, 0, sinks)
}

fn encode_audio(
    audio: &mut AudioEncoder,
    chunk: AudioChunk,
    origin: Option<Instant>,
    sinks: &mut Sinks,
) -> Result<()> {
    // Audio is placed relative to the first video frame; drop anything earlier.
    let Some(origin) = origin else { return Ok(()) };
    let offset = if chunk.at >= origin {
        (chunk.at - origin).as_secs_f64()
    } else {
        -(origin - chunk.at).as_secs_f64()
    };
    let frames = chunk.samples.len() / RECORD_CHANNELS;
    let placement = audio.clock.place(offset, frames);
    audio.fifo_pts.get_or_insert(placement.pts);
    audio.fifo.resize(audio.fifo.len() + placement.pad * RECORD_CHANNELS, 0.0);
    audio.fifo.extend_from_slice(&chunk.samples[placement.skip * RECORD_CHANNELS..]);

    let chunk_len = audio.frame_size * RECORD_CHANNELS;
    while audio.fifo.len() >= chunk_len {
        let mut frame = frame::Audio::new(audio.sample, audio.frame_size, ChannelLayout::default(2));
        frame.set_rate(audio.rate);
        fill_audio(&mut frame, audio.sample, &audio.fifo[..chunk_len]);
        let pts = audio.fifo_pts.unwrap();
        frame.set_pts(Some(pts));
        audio.fifo_pts = Some(pts + audio.frame_size as i64);
        audio.fifo.drain(..chunk_len);
        audio.encoder.send_frame(&frame)?;
        drain(&mut audio.encoder, 1, sinks)?;
    }
    Ok(())
}

/// Writes interleaved stereo f32 into a frame of the encoder's sample format.
fn fill_audio(frame: &mut frame::Audio, format: Sample, samples: &[f32]) {
    match format {
        Sample::I16(sample::Type::Packed) => {
            for (dst, s) in frame.data_mut(0).chunks_exact_mut(2).zip(samples) {
                dst.copy_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_ne_bytes());
            }
        }
        Sample::F32(sample::Type::Planar) => {
            for channel in 0..RECORD_CHANNELS {
                let plane = frame.data_mut(channel);
                for (dst, s) in plane.chunks_exact_mut(4).zip(samples.iter().skip(channel).step_by(RECORD_CHANNELS)) {
                    dst.copy_from_slice(&s.to_ne_bytes());
                }
            }
        }
        other => unreachable!("audio encoders are opened as s16 or fltp, not {other:?}"),
    }
}

fn drain(
    encoder: &mut encoder::Encoder,
    stream: usize,
    sinks: &mut Sinks,
) -> Result<()> {
    let mut packet = Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        sinks.emit(stream, &packet)?;
    }
    Ok(())
}

fn open_video(codec: Codec, format: &VideoFormat, tb: Rational) -> Result<(VideoEncoder, String)> {
    let (width, height) = (format.width, format.height);
    let mut errors = Vec::new();
    for &name in codec.encoders() {
        let Some(ff_codec) = encoder::find_by_name(name) else { continue };
        let supported: Option<Vec<Pixel>> =
            ff_codec.video().ok().and_then(|v| v.formats()).map(|f| f.collect());
        let pixel = codec
            .preferred_pixels()
            .iter()
            .copied()
            .find(|p| supported.as_ref().is_none_or(|s| s.contains(p)))
            .or_else(|| supported.as_ref().and_then(|s| s.first().copied()));
        let Some(pixel) = pixel else { continue };

        let mut ctx = codec::context::Context::new_with_codec(ff_codec).encoder().video()?;
        ctx.set_width(width);
        ctx.set_height(height);
        ctx.set_format(pixel);
        ctx.set_time_base(tb);
        ctx.set_frame_rate(Some(Rational(tb.1, tb.0)));
        ctx.set_gop(codec.gop(format.fps));
        ctx.set_max_b_frames(0);
        if let Some(rate) = codec.bit_rate(width, height, format.fps) {
            ctx.set_bit_rate(rate);
        }
        // Codec headers go in the parameters, which the DVR player and any
        // later-started recording both need.
        ctx.set_flags(codec::Flags::GLOBAL_HEADER);
        ctx.set_threading(threading::Config::count(0));
        if pixel != Pixel::BGRZ {
            // swscale's default RGB→YUV matrix is BT.601 limited range; say so in
            // the file so players don't have to guess.
            ctx.set_color_range(color::Range::MPEG);
            ctx.set_colorspace(color::Space::SMPTE170M);
            ctx.set_color_primaries(color::Primaries::SMPTE170M);
            ctx.set_color_transfer_characteristic(color::TransferCharacteristic::SMPTE170M);
        }

        match ctx.open_with(dictionary(&codec.options(name))) {
            Ok(encoder) => {
                let scaler = scaling::Context::get(
                    Pixel::BGRA,
                    width,
                    height,
                    pixel,
                    width,
                    height,
                    scaling::Flags::BILINEAR | scaling::Flags::ACCURATE_RND,
                )?;
                let mut converted = frame::Video::new(pixel, width, height);
                if pixel != Pixel::BGRZ {
                    converted.set_color_range(color::Range::MPEG);
                }
                let video = VideoEncoder {
                    encoder,
                    scaler,
                    bgra: frame::Video::new(Pixel::BGRA, width, height),
                    converted,
                    clock: FrameClock::new(tb),
                    width,
                    height,
                };
                return Ok((video, name.to_string()));
            }
            Err(err) => errors.push(format!("{name}: {err}")),
        }
    }
    Err(anyhow!(
        "no {} encoder available{}",
        codec.label(),
        if errors.is_empty() { String::new() } else { format!(" ({})", errors.join("; ")) }
    ))
}

fn open_audio(codec: Codec, rate: u32, video_tb: Rational) -> Result<AudioEncoder> {
    let name = codec.audio_encoder();
    let ff_codec = encoder::find_by_name(name).with_context(|| format!("no {name} encoder"))?;
    let sample = match name {
        "aac" => Sample::F32(sample::Type::Planar),
        _ => Sample::I16(sample::Type::Packed),
    };
    let mut ctx = codec::context::Context::new_with_codec(ff_codec).encoder().audio()?;
    ctx.set_rate(rate as i32);
    ctx.set_channel_layout(ChannelLayout::default(RECORD_CHANNELS as i32));
    ctx.set_format(sample);
    ctx.set_time_base(Rational(1, rate as i32));
    ctx.set_flags(codec::Flags::GLOBAL_HEADER);
    if name == "aac" {
        ctx.set_bit_rate(256_000);
    }
    let encoder = ctx.open_with(Dictionary::new()).with_context(|| format!("opening {name}"))?;
    let frame_size = match encoder.frame_size() {
        0 => 1024,
        n => n as usize,
    };
    // Resync audio once it's more than a video frame off.
    let frame_secs = video_tb.0 as f64 / video_tb.1 as f64;
    let tolerance = (frame_secs * rate as f64).round() as i64;
    Ok(AudioEncoder {
        encoder,
        sample,
        frame_size,
        clock: AudioClock::new(rate, tolerance),
        fifo: Vec::new(),
        fifo_pts: None,
        rate,
    })
}

fn dictionary<'a>(options: &[(&str, &str)]) -> Dictionary<'a> {
    let mut dict = Dictionary::new();
    for (key, value) in options {
        dict.set(key, value);
    }
    dict
}

/// Copies tightly packed BGRA rows into a frame whose rows may be padded.
fn copy_into(src: &[u8], width: u32, dst: &mut frame::Video) {
    let row = width as usize * 4;
    let stride = dst.stride(0);
    for (src_row, dst_row) in src.chunks_exact(row).zip(dst.data_mut(0).chunks_mut(stride)) {
        dst_row[..row].copy_from_slice(src_row);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use gpui::RenderImage;
    use image::RgbaImage;
    use std::path::Path;
    use std::sync::Arc;

    /// Draws `n` as 16 black/white bands, one bit each, so decoders can be checked.
    pub(crate) fn numbered_frame(n: u32, width: u32, height: u32) -> Arc<RenderImage> {
        let band = width / 16;
        let image = RgbaImage::from_fn(width, height, |x, _| {
            let bit = (n >> (x / band).min(15)) & 1;
            image::Rgba(if bit == 1 { [255, 255, 255, 255] } else { [0, 0, 0, 255] })
        });
        Arc::new(RenderImage::new([image::Frame::new(image)]))
    }

    /// Reads the number back from BGRA pixels drawn by `numbered_frame`.
    pub(crate) fn read_number(bgra: &[u8], width: u32) -> u32 {
        let band = width / 16;
        let row = (width * 4 * 10) as usize; // sample row 10, away from edges
        (0..16)
            .map(|bit| {
                let x = (bit * band + band / 2) as usize;
                let luma = bgra[row + x * 4 + 1];
                ((luma > 128) as u32) << bit
            })
            .sum()
    }

    pub(crate) struct Synthetic {
        pub pipeline: Pipeline,
        pub store: Store,
        pub events: futures::channel::mpsc::UnboundedReceiver<PipelineEvent>,
        pub taps: Taps,
        pub t0: Instant,
    }

    /// Starts a pipeline and waits until it's ready.
    pub(crate) fn synthetic_pipeline(codec: Codec, fps: f64, audio: bool) -> Synthetic {
        let format = VideoFormat { width: 720, height: 480, fps, pixel: "test".into() };
        let (tx, mut events) = futures::channel::mpsc::unbounded();
        let audio_rate = audio.then_some(48_000);
        let (pipeline, taps) =
            Pipeline::start(codec, format, audio_rate, Duration::from_secs(600), tx);
        let store = loop {
            match events.try_recv() {
                Ok(PipelineEvent::Ready { encoder, store }) => {
                    println!("{}: using {encoder}", codec.label());
                    break store;
                }
                Ok(PipelineEvent::Error(err) | PipelineEvent::Failed(err)) => {
                    panic!("{}: {err}", codec.label())
                }
                _ => thread::sleep(Duration::from_millis(5)),
            }
        };
        Synthetic { pipeline, store, events, taps, t0: Instant::now() }
    }

    impl Synthetic {
        /// Feeds frames `range` (numbered) plus a 1 kHz tone covering them.
        pub(crate) fn feed(&self, range: std::ops::Range<u32>, fps: f64) {
            for i in range {
                let at = self.t0 + Duration::from_secs_f64(i as f64 / fps);
                let image = numbered_frame(i, 720, 480);
                self.taps.video.send(TapFrame { image, at }).unwrap();
                if let Some(audio) = &self.taps.audio {
                    let frames = (48_000.0 / fps).round() as usize;
                    let samples = (0..frames)
                        .flat_map(|n| {
                            let t = (i as f64 / fps) + n as f64 / 48_000.0;
                            let s = (t * 1000.0 * std::f64::consts::TAU).sin() as f32 * 0.5;
                            [s, s]
                        })
                        .collect();
                    audio.send(AudioChunk { samples, at }).unwrap();
                }
            }
        }

        /// Drops the taps and waits for the pipeline; returns the events it sent.
        pub(crate) fn finish(self) -> (Store, Vec<PipelineEvent>) {
            drop(self.taps);
            self.pipeline.join();
            let mut events = self.events;
            let mut out = Vec::new();
            while let Ok(e) = events.try_recv() {
                out.push(e);
            }
            (self.store, out)
        }
    }

    fn ffprobe(path: &Path, stream: &str, entries: &str) -> String {
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", stream, "-count_frames"])
            .args(["-show_entries", entries, "-of", "csv=p=0"])
            .arg(path)
            .output()
            .expect("ffprobe on PATH");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn saved_path(events: &[PipelineEvent]) -> PathBuf {
        for e in events {
            match e {
                PipelineEvent::Error(err) | PipelineEvent::Failed(err) => panic!("{err}"),
                PipelineEvent::Saved { path, .. } => return path.clone(),
                _ => {}
            }
        }
        panic!("no recording saved");
    }

    #[test]
    #[ignore = "needs FFmpeg encoders; run with -- --ignored --nocapture"]
    fn every_codec_records_video_and_audio() {
        let dir = std::env::temp_dir().join(format!("frameback-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for codec in Codec::ALL {
            let s = synthetic_pipeline(codec, 59.94, true);
            s.feed(0..30, 59.94);
            // Recording starts mid-stream, as when pressing R during play.
            let path = dir.join(format!("synthetic.{}", codec.extension()));
            s.pipeline.record(path, s.t0 + Duration::from_secs_f64(30.0 / 59.94));
            s.feed(30..150, 59.94);
            s.pipeline.stop_recording();
            s.feed(150..160, 59.94);
            let (_, events) = s.finish();
            let path = saved_path(&events);

            let video = ffprobe(&path, "v:0", "stream=codec_name,r_frame_rate,nb_read_frames");
            let audio = ffprobe(&path, "a:0", "stream=codec_name,sample_rate,channels");
            let starts = ffprobe(&path, "v:0", "stream=start_time");
            let audio_start = ffprobe(&path, "a:0", "stream=start_time");
            println!("{}: video {video} | audio {audio} | start v {starts} a {audio_start}", codec.label());

            let fields: Vec<&str> = video.split(',').collect();
            assert_eq!(fields[1], "60000/1001", "{}: frame rate", codec.label());
            // Starts at the keyframe at or before frame 30 that's buffered by
            // the time the command is handled; ends at frame 149.
            let frames: i64 = fields[2].parse().unwrap();
            assert!((120..=150).contains(&frames), "{}: {frames} frames", codec.label());
            assert!(audio.ends_with(",48000,2"), "{}: audio {audio}", codec.label());
            let (v, a): (f64, f64) = (starts.parse().unwrap(), audio_start.parse().unwrap());
            assert!((v - a).abs() < 1.0 / 59.94 + 0.03, "{}: A/V start offset {v} vs {a}", codec.label());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "records a real device + its audio (FRAMEBACK_TEST_DEVICE=name); -- --ignored --nocapture"]
    fn records_a_device_with_audio() {
        use crate::audio::{AudioTap, Monitor, Volume, list_inputs};
        use crate::record::codec::effective_fps;
        use crate::video::{Capture, FrameSlot, VideoEvent, list_devices};

        let wanted = std::env::var("FRAMEBACK_TEST_DEVICE").unwrap_or_default();
        let device = list_devices().into_iter().find(|d| d.name.contains(&wanted)).expect("device");
        let input = list_inputs().into_iter().find(|a| a.name == device.name).expect("audio input");
        let tap = AudioTap::default();
        let monitor = Monitor::start(&input, Volume::new(0.0), tap.clone()).unwrap();

        let slot = Arc::new(FrameSlot::default());
        let (video_tx, mut video_rx) = futures::channel::mpsc::unbounded();
        let _capture = Capture::start(device.clone(), None, slot.clone(), video_tx, None);
        let mut format = loop {
            match video_rx.try_recv() {
                Ok(VideoEvent::Opened { active, .. }) => break active,
                Ok(VideoEvent::Error(err)) => panic!("{err}"),
                _ => thread::sleep(Duration::from_millis(10)),
            }
        };
        let before = slot.delivered();
        thread::sleep(Duration::from_secs(1));
        format.fps = effective_fps(format.fps, Some((slot.delivered() - before) as f64));

        let (tx, mut events) = futures::channel::mpsc::unbounded();
        let (pipeline, taps) =
            Pipeline::start(Codec::Ffv1, format.clone(), Some(monitor.sample_rate), Duration::from_secs(60), tx);
        slot.set_tap(Some(taps.video));
        tap.set(taps.audio);
        thread::sleep(Duration::from_secs(1));
        let path = std::env::temp_dir().join("frameback-device-audio.mkv");
        pipeline.record(path, Instant::now());
        thread::sleep(Duration::from_secs(3));
        pipeline.stop_recording();
        slot.set_tap(None);
        tap.set(None);
        pipeline.join();

        let mut all = Vec::new();
        while let Ok(e) = events.try_recv() {
            all.push(e);
        }
        let path = saved_path(&all);
        let video = ffprobe(&path, "v:0", "stream=r_frame_rate,nb_read_frames,start_time");
        let audio = ffprobe(&path, "a:0", "stream=codec_name,sample_rate,start_time,duration");
        println!("{} ({}): video {video} | audio {audio} | dropped {}", device.name, format.label(), slot.tap_dropped());
        let frames: f64 = ffprobe(&path, "v:0", "stream=nb_read_frames").parse().unwrap();
        assert!((frames - 3.0 * format.fps).abs() < format.fps * 0.25, "{frames} frames");
        // MKV doesn't store per-stream durations; use the last audio packet's end.
        let packets = ffprobe(&path, "a:0", "packet=pts_time,duration_time");
        let last: Vec<f64> =
            packets.lines().last().unwrap().split(',').map(|v| v.parse().unwrap()).collect();
        let audio_secs = last[0] + last[1];
        println!("audio ends at {audio_secs:.3}s");
        assert!((audio_secs - 3.0).abs() < 0.3, "audio lasts {audio_secs}s");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    #[ignore = "needs FFmpeg encoders; run with -- --ignored --nocapture"]
    fn frozen_buffer_is_not_extended_or_overwritten() {
        let fps = 59.94;
        let s = synthetic_pipeline(Codec::Hevc, fps, false);
        s.feed(0..60, fps);
        // Let the pipeline catch up before freezing, as a user would.
        thread::sleep(Duration::from_millis(300));
        s.pipeline.set_frozen(true);
        thread::sleep(Duration::from_millis(50));
        let frozen = s.store.video_range().unwrap();
        s.feed(60..120, fps);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(s.store.video_range().unwrap(), frozen, "buffer changed while frozen");

        s.pipeline.set_frozen(false);
        s.feed(120..180, fps);
        let (store, _) = s.finish();
        let (start, end) = store.video_range().unwrap();
        assert_eq!(start, frozen.0, "old footage kept");
        assert!(end > frozen.1 + 0.9, "buffering resumed");
        // Resumes on a keyframe: the first frame after the gap must be one.
        let video = store.stream_of(StreamKind::Video).unwrap();
        let desc = &store.streams()[video];
        let after_gap = store.next_after(video, desc.pts(frozen.1)).unwrap();
        assert!(after_gap.key, "resumed mid-GOP");
        assert!(desc.seconds(after_gap.pts) >= 120.0 / fps - 0.01);
    }

    #[test]
    fn frames_snap_to_grid() {
        let mut clock = FrameClock::new(Rational(1, 60));
        let t0 = Instant::now();
        let ms = |n: f64| t0 + Duration::from_secs_f64(n / 1000.0);
        assert_eq!(clock.index(t0), Some(0));
        assert_eq!(clock.index(ms(17.5)), Some(1), "jitter rounds to the nearest slot");
        assert_eq!(clock.index(ms(20.0)), None, "second frame in slot 1 is dropped");
        assert_eq!(clock.index(ms(66.0)), Some(4), "a gap skips slots 2 and 3");
    }

    #[test]
    fn audio_stays_continuous_within_tolerance() {
        let mut clock = AudioClock::new(48_000, 800);
        assert_eq!(clock.place(0.010, 480), Placement { pts: 480, skip: 0, pad: 0 });
        // Timestamp jitter of a few ms doesn't break continuity.
        assert_eq!(clock.place(0.023, 480), Placement { pts: 960, skip: 0, pad: 0 });
    }

    #[test]
    fn audio_resyncs_when_drift_exceeds_tolerance() {
        let mut clock = AudioClock::new(48_000, 800);
        clock.place(0.0, 480);
        // Next chunk arrives 100 ms later than its samples imply: pad with silence.
        assert_eq!(clock.place(0.110, 480), Placement { pts: 480, skip: 0, pad: 4800 });
        // Then one that's 50 ms early: drop its leading samples.
        let next = 480 + 4800 + 480;
        let early = (next - 2400) as f64 / 48_000.0;
        assert_eq!(clock.place(early, 4800), Placement { pts: next, skip: 2400, pad: 0 });
    }

    #[test]
    fn audio_before_the_first_frame_is_trimmed() {
        let mut clock = AudioClock::new(48_000, 800);
        assert_eq!(clock.place(-0.005, 480), Placement { pts: 0, skip: 240, pad: 0 });
        assert_eq!(clock.place(0.005, 480), Placement { pts: 240, skip: 0, pad: 0 });
    }
}
