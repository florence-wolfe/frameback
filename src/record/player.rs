//! DVR playback: shows any buffered frame, plays forward at 1×, steps frames.
//!
//! Frames go to their own `FrameSlot`, so the UI swaps between live and
//! playback without either side touching the other's pipeline.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::codec::{self, threading};
use ffmpeg::format::{Pixel, Sample, sample};
use ffmpeg::software::scaling;
use ffmpeg::{Packet, decoder, frame};
use futures::channel::mpsc::UnboundedSender;
use gpui::RenderImage;
use image::{Frame, RgbaImage};

use super::store::{Entry, Store, StoreReader, StreamKind};
use crate::audio::Playback;
use crate::video::{FrameSlot, LatestFrame};

/// How far ahead of the picture decoded audio is queued.
const AUDIO_LEAD: f64 = 0.15;
/// A jump between consecutive frames larger than this is a gap in the buffer
/// (it was frozen for review); playback cuts across it instead of waiting.
const MAX_GAP: f64 = 0.5;

pub enum PlayerEvent {
    /// A frame for `time` (session seconds) is waiting in the playback slot.
    Frame { time: f64 },
    /// Playback reached the newest buffered frame.
    CaughtUp,
    Error(String),
}

enum Command {
    Seek(f64),
    Play,
    Pause,
    Step(i64),
}

/// Dropping the player stops its thread and hands audio back to the live input.
pub struct Player {
    commands: mpsc::Sender<Command>,
    _thread: JoinHandle<()>,
}

impl Player {
    pub fn start(
        store: Store,
        slot: Arc<FrameSlot>,
        audio: Option<Arc<Playback>>,
        events: UnboundedSender<PlayerEvent>,
    ) -> Player {
        let (commands, rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("frameback-player".into())
            .spawn(move || {
                if let Some(audio) = &audio {
                    audio.set_active(true);
                }
                let result = State::new(store, slot, audio.clone(), events.clone())
                    .and_then(|state| state.run(rx));
                if let Err(err) = result {
                    let _ = events.unbounded_send(PlayerEvent::Error(format!("{err:#}")));
                }
                if let Some(audio) = &audio {
                    audio.set_active(false);
                }
            })
            .expect("spawn player thread");
        Player { commands, _thread: thread }
    }

    pub fn seek(&self, time: f64) {
        let _ = self.commands.send(Command::Seek(time));
    }
    pub fn play(&self) {
        let _ = self.commands.send(Command::Play);
    }
    pub fn pause(&self) {
        let _ = self.commands.send(Command::Pause);
    }
    pub fn step(&self, frames: i64) {
        let _ = self.commands.send(Command::Step(frames));
    }
}

struct PlayClock {
    wall: Instant,
    time: f64,
}

struct AudioState {
    stream: usize,
    decoder: decoder::Audio,
    sink: Arc<Playback>,
    /// Next audio pts to decode.
    cursor: i64,
}

struct State {
    store: Store,
    reader: StoreReader,
    slot: Arc<FrameSlot>,
    events: UnboundedSender<PlayerEvent>,
    stream: usize,
    decoder: decoder::Video,
    to_bgra: ToBgra,
    /// Frame on screen.
    shown: Option<i64>,
    /// Last packet fed to the decoder, if the decoder state follows on from it.
    decoded: Option<i64>,
    playing: Option<PlayClock>,
    audio: Option<AudioState>,
}

fn open_decoder(store: &Store, stream: usize) -> Result<codec::decoder::Decoder> {
    let mut ctx = codec::context::Context::from_parameters(store.streams()[stream].params.get())?;
    // No frame threading: each packet must come out as its frame right away,
    // or seeking would land a few frames short.
    ctx.set_threading(threading::Config::kind(threading::Type::Slice));
    ctx.set_flags(codec::Flags::LOW_DELAY);
    Ok(ctx.decoder())
}

impl State {
    fn new(
        store: Store,
        slot: Arc<FrameSlot>,
        sink: Option<Arc<Playback>>,
        events: UnboundedSender<PlayerEvent>,
    ) -> Result<State> {
        let stream = store.stream_of(StreamKind::Video).context("no video in DVR buffer")?;
        let decoder = open_decoder(&store, stream)?.video().context("opening video decoder")?;
        let audio = match (store.stream_of(StreamKind::Audio), sink) {
            (Some(stream), Some(sink)) => Some(AudioState {
                stream,
                decoder: open_decoder(&store, stream)?.audio().context("opening audio decoder")?,
                sink,
                cursor: 0,
            }),
            _ => None,
        };
        Ok(State {
            reader: store.reader(),
            store,
            slot,
            events,
            stream,
            decoder,
            to_bgra: ToBgra::default(),
            shown: None,
            decoded: None,
            playing: None,
            audio,
        })
    }

    fn seconds(&self, pts: i64) -> f64 {
        self.store.streams()[self.stream].seconds(pts)
    }

    fn run(mut self, commands: Receiver<Command>) -> Result<()> {
        loop {
            let command = match self.next_due() {
                Some(due) => {
                    match commands.recv_timeout(due.saturating_duration_since(Instant::now())) {
                        Ok(command) => command,
                        Err(RecvTimeoutError::Timeout) => {
                            self.advance()?;
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                }
                None => match commands.recv() {
                    Ok(command) => command,
                    Err(_) => return Ok(()),
                },
            };
            // Only the newest seek matters; drop older ones queued behind it.
            let mut command = command;
            while let Ok(next) = commands.try_recv() {
                match (&command, &next) {
                    (Command::Seek(_), Command::Seek(_)) => command = next,
                    _ => {
                        self.handle(command)?;
                        command = next;
                    }
                }
            }
            self.handle(command)?;
        }
    }

    fn handle(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Seek(time) => {
                let pts = self.store.streams()[self.stream].pts(time);
                self.show(pts)?;
                if self.playing.is_some() {
                    self.start_clock();
                }
            }
            Command::Play => self.start_clock(),
            Command::Pause => {
                self.playing = None;
                if let Some(audio) = &self.audio {
                    audio.sink.flush();
                }
            }
            Command::Step(n) => {
                self.playing = None;
                if let Some(audio) = &self.audio {
                    audio.sink.flush();
                }
                let from = self.shown.unwrap_or(i64::MAX);
                if let Some(target) = self.store.offset_from(self.stream, from, n) {
                    self.show(target.pts)?;
                }
            }
        }
        Ok(())
    }

    fn start_clock(&mut self) {
        let Some(shown) = self.shown else { return };
        let time = self.seconds(shown);
        self.playing = Some(PlayClock { wall: Instant::now(), time });
        if let Some(audio) = &mut self.audio {
            audio.sink.flush();
            audio.cursor = self.store.streams()[audio.stream].pts(time);
        }
    }

    /// When the next frame is due, while playing.
    fn next_due(&self) -> Option<Instant> {
        let clock = self.playing.as_ref()?;
        let shown = self.shown?;
        let next = self.store.next_after(self.stream, shown);
        let time = next.map_or(clock.time, |e| self.seconds(e.pts));
        if time - self.seconds(shown) > MAX_GAP {
            return Some(Instant::now());
        }
        Some(clock.wall + Duration::from_secs_f64((time - clock.time).max(0.0)))
    }

    /// Shows the next frame while playing, and keeps audio queued ahead.
    fn advance(&mut self) -> Result<()> {
        let Some(shown) = self.shown else { return Ok(()) };
        match self.store.next_after(self.stream, shown) {
            Some(next) => {
                let gap = self.seconds(next.pts) - self.seconds(shown) > MAX_GAP;
                self.show(next.pts)?;
                if gap {
                    // Restart the clock (and audio) on the far side of the gap.
                    self.start_clock();
                }
                self.feed_audio(self.seconds(next.pts) + AUDIO_LEAD)?;
            }
            None => {
                self.playing = None;
                let _ = self.events.unbounded_send(PlayerEvent::CaughtUp);
            }
        }
        Ok(())
    }

    /// Decodes and publishes the frame at (or just before) `pts`.
    fn show(&mut self, pts: i64) -> Result<()> {
        let run = self.store.decode_run(self.stream, pts);
        let Some(target) = run.last().copied() else { return Ok(()) };
        // Continue from the decoder's current position when it's inside this run.
        let resume = self.decoded.and_then(|d| run.iter().position(|e| e.pts == d));
        let packets: &[Entry] = match resume {
            Some(i) if run[i].pts < target.pts => &run[i + 1..],
            Some(i) if run[i].pts == target.pts && self.shown == Some(target.pts) => return Ok(()),
            _ => {
                self.decoder.flush();
                &run
            }
        };

        let mut decoded = frame::Video::empty();
        let mut last = None;
        for entry in packets.to_vec() {
            let data = self.reader.read(&entry)?;
            let mut packet = Packet::copy(&data);
            packet.set_pts(Some(entry.pts));
            packet.set_dts(Some(entry.pts));
            if entry.key {
                packet.set_flags(codec::packet::Flags::KEY);
            }
            self.decoder.send_packet(&packet)?;
            self.decoded = Some(entry.pts);
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                last = Some(decoded.clone());
            }
        }
        let Some(frame) = last else { return Ok(()) };
        let image = self.to_bgra.convert(&frame)?;
        self.shown = Some(target.pts);
        let time = self.seconds(target.pts);
        self.slot.show(LatestFrame { image, captured_at: None });
        let _ = self.events.unbounded_send(PlayerEvent::Frame { time });
        Ok(())
    }

    /// Decodes audio up to `until` (session seconds) into the output queue.
    fn feed_audio(&mut self, until: f64) -> Result<()> {
        let Some(audio) = &mut self.audio else { return Ok(()) };
        let desc = &self.store.streams()[audio.stream];
        let until = desc.pts(until);
        if audio.cursor >= until {
            return Ok(());
        }
        let mut decoded = frame::Audio::empty();
        for entry in self.store.range(audio.stream, audio.cursor, until) {
            let data = self.reader.read(&entry)?;
            let mut packet = Packet::copy(&data);
            packet.set_pts(Some(entry.pts));
            audio.decoder.send_packet(&packet)?;
            while audio.decoder.receive_frame(&mut decoded).is_ok() {
                let samples = interleave(&decoded, audio.sink.channels);
                audio.sink.push(&samples);
            }
            audio.cursor = entry.pts + entry.duration;
        }
        Ok(())
    }
}

/// Converts decoded frames to the BGRA images the UI shows, reusing the
/// scaler while the frame format stays the same.
#[derive(Default)]
pub(super) struct ToBgra(Option<(scaling::Context, Pixel, u32, u32)>);

impl ToBgra {
    pub(super) fn convert(&mut self, frame: &frame::Video) -> Result<Arc<RenderImage>> {
        let (format, width, height) = (frame.format(), frame.width(), frame.height());
        if !self.0.as_ref().is_some_and(|(_, f, w, h)| (*f, *w, *h) == (format, width, height)) {
            let scaler = scaling::Context::get(
                format,
                width,
                height,
                Pixel::BGRA,
                width,
                height,
                scaling::Flags::BILINEAR | scaling::Flags::ACCURATE_RND,
            )?;
            self.0 = Some((scaler, format, width, height));
        }
        let mut bgra = frame::Video::new(Pixel::BGRA, width, height);
        self.0.as_mut().unwrap().0.run(frame, &mut bgra)?;
        let row = width as usize * 4;
        let stride = bgra.stride(0);
        let mut pixels = Vec::with_capacity(row * height as usize);
        for line in bgra.data(0).chunks(stride).take(height as usize) {
            pixels.extend_from_slice(&line[..row]);
        }
        let image = RgbaImage::from_raw(width, height, pixels).context("frame size")?;
        Ok(Arc::new(RenderImage::new([Frame::new(image)])))
    }
}

/// Converts a decoded audio frame to interleaved f32 with `channels` channels.
fn interleave(frame: &frame::Audio, channels: usize) -> Vec<f32> {
    let n = frame.samples();
    let source_channels = frame.channels().max(1) as usize;
    let sample = |ch: usize, i: usize| -> f32 {
        let ch = ch.min(source_channels - 1);
        match frame.format() {
            Sample::I16(sample::Type::Packed) => {
                let b = &frame.data(0)[(i * source_channels + ch) * 2..][..2];
                i16::from_ne_bytes([b[0], b[1]]) as f32 / 32768.0
            }
            Sample::I16(sample::Type::Planar) => {
                let b = &frame.data(ch)[i * 2..][..2];
                i16::from_ne_bytes([b[0], b[1]]) as f32 / 32768.0
            }
            Sample::F32(sample::Type::Packed) => {
                let b = &frame.data(0)[(i * source_channels + ch) * 4..][..4];
                f32::from_ne_bytes([b[0], b[1], b[2], b[3]])
            }
            Sample::F32(sample::Type::Planar) => {
                let b = &frame.data(ch)[i * 4..][..4];
                f32::from_ne_bytes([b[0], b[1], b[2], b[3]])
            }
            _ => 0.0,
        }
    };
    (0..n).flat_map(|i| (0..channels).map(move |c| sample(c, i))).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::record::codec::Codec;
    use crate::record::pipeline::tests::{read_number, synthetic_pipeline};

    /// Waits for the next frame event and returns the number drawn in it.
    pub(crate) fn next_frame(
        events: &mut futures::channel::mpsc::UnboundedReceiver<PlayerEvent>,
        slot: &FrameSlot,
    ) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match events.try_recv() {
                Ok(PlayerEvent::Frame { .. }) => {
                    let frame = slot.take().expect("frame in slot");
                    return read_number(frame.image.as_bytes(0).unwrap(), 720);
                }
                Ok(PlayerEvent::Error(err)) => panic!("{err}"),
                Ok(PlayerEvent::CaughtUp) => {}
                Err(_) if Instant::now() > deadline => panic!("no frame"),
                Err(_) => thread::sleep(Duration::from_millis(2)),
            }
        }
    }

    #[test]
    #[ignore = "needs FFmpeg; run with -- --ignored --nocapture"]
    fn seeks_and_steps_are_frame_accurate() {
        let fps = 59.94;
        for codec in Codec::ALL {
            let s = synthetic_pipeline(codec, fps, false);
            s.feed(0..300, fps);
            let (store, _) = s.finish();
            let slot = Arc::new(FrameSlot::default());
            let (tx, mut events) = futures::channel::mpsc::unbounded();
            let player = Player::start(store, slot.clone(), None, tx);
            let time = |n: u32| n as f64 * 1001.0 / 60000.0;

            // Random-ish targets, including mid-GOP for the lossy codecs.
            for n in [0, 137, 7, 299, 150, 151, 64] {
                player.seek(time(n));
                assert_eq!(next_frame(&mut events, &slot), n, "{}: seek to {n}", codec.label());
            }
            player.step(1);
            assert_eq!(next_frame(&mut events, &slot), 65, "{}: step +1", codec.label());
            player.step(-2);
            assert_eq!(next_frame(&mut events, &slot), 63, "{}: step -2", codec.label());
            println!("{}: seeks and steps exact", codec.label());
        }
    }

    #[test]
    #[ignore = "needs FFmpeg; run with -- --ignored --nocapture"]
    fn playback_cuts_across_a_frozen_gap() {
        let fps = 59.94;
        let s = synthetic_pipeline(Codec::Hevc, fps, false);
        s.feed(0..60, fps);
        thread::sleep(Duration::from_millis(300));
        s.pipeline.set_frozen(true);
        thread::sleep(Duration::from_millis(50));
        s.feed(60..120, fps);
        thread::sleep(Duration::from_millis(300));
        s.pipeline.set_frozen(false);
        s.feed(120..180, fps);
        let (store, _) = s.finish();

        let slot = Arc::new(FrameSlot::default());
        let (tx, mut events) = futures::channel::mpsc::unbounded();
        let player = Player::start(store, slot.clone(), None, tx);
        player.seek(50.0 * 1001.0 / 60000.0);
        assert_eq!(next_frame(&mut events, &slot), 50);
        let started = Instant::now();
        player.play();
        let mut last = 50;
        loop {
            match events.try_recv() {
                Ok(PlayerEvent::Frame { .. }) => {
                    if let Some(f) = slot.take() {
                        last = read_number(f.image.as_bytes(0).unwrap(), 720);
                    }
                }
                Ok(PlayerEvent::CaughtUp) => break,
                Ok(PlayerEvent::Error(err)) => panic!("{err}"),
                Err(_) => thread::sleep(Duration::from_millis(1)),
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        assert_eq!(last, 179);
        // ~70 buffered frames after 50 (the gap frames were never stored).
        assert!(elapsed < 1.5, "waited out the gap: {elapsed:.2}s");
    }

    #[test]
    #[ignore = "needs FFmpeg; run with -- --ignored --nocapture"]
    fn plays_forward_in_real_time_and_catches_up() {
        let fps = 59.94;
        let s = synthetic_pipeline(Codec::H264, fps, false);
        s.feed(0..120, fps);
        let (store, _) = s.finish();
        let slot = Arc::new(FrameSlot::default());
        let (tx, mut events) = futures::channel::mpsc::unbounded();
        let player = Player::start(store, slot.clone(), None, tx);
        player.seek(60.0 * 1001.0 / 60000.0);
        assert_eq!(next_frame(&mut events, &slot), 60);
        let started = Instant::now();
        player.play();
        let mut seen = Vec::new();
        loop {
            match events.try_recv() {
                Ok(PlayerEvent::Frame { .. }) => {
                    if let Some(f) = slot.take() {
                        seen.push(read_number(f.image.as_bytes(0).unwrap(), 720));
                    }
                }
                Ok(PlayerEvent::CaughtUp) => break,
                Ok(PlayerEvent::Error(err)) => panic!("{err}"),
                Err(_) => thread::sleep(Duration::from_millis(1)),
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        // 59 frames after frame 60 take ~0.98 s at 59.94 fps.
        assert!((elapsed - 59.0 / fps).abs() < 0.1, "played in {elapsed:.3}s");
        assert_eq!(seen.last(), Some(&119));
        assert!(seen.windows(2).all(|w| w[1] > w[0]), "frames in order");
    }
}
