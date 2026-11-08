//! Importing a video file into a DVR buffer, so it can be reviewed, stepped
//! and clipped like captured footage.
//!
//! The file is decoded and fed through the same encoder pipeline a capture
//! device uses, so the buffer holds the selected codec without B-frames:
//! exactly what the player and clip saver expect, whatever the file contains.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg::codec::{self, threading};
use ffmpeg::format::{Sample, sample};
use ffmpeg::software::resampling;
use ffmpeg::{ChannelLayout, Rational, decoder, frame, media};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;

use super::codec::Codec;
use super::pipeline::{Pipeline, PipelineEvent};
use super::player::ToBgra;
use super::store::Store;
use crate::audio::{AudioChunk, RECORD_CHANNELS};
use crate::video::{TapFrame, VideoFormat};

pub enum Source {
    File(PathBuf),
    /// Downloaded (with yt-dlp) into the folder, then imported.
    Url { url: String, into: PathBuf },
}

pub enum ImportEvent {
    /// Fraction of a URL's download done so far.
    Downloading(f32),
    /// The download finished and conversion is starting.
    Downloaded { path: PathBuf },
    /// Fraction of the file converted so far.
    Progress(f32),
    Done { store: Store },
    Failed(String),
}

/// Dropping it cancels an import still in progress.
pub struct Import {
    cancel: Arc<AtomicBool>,
}

impl Drop for Import {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Import {
    /// Converts the source into a buffer of `codec`. Audio is resampled to
    /// `playback_rate` (the output the player feeds) or kept at the file's rate.
    pub fn start(
        source: Source,
        codec: Codec,
        playback_rate: Option<u32>,
        events: UnboundedSender<ImportEvent>,
    ) -> Import {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = cancel.clone();
        thread::Builder::new()
            .name("frameback-import".into())
            .spawn(move || {
                let path = match source {
                    Source::File(path) => path,
                    Source::Url { url, into } => {
                        let progress = |p| {
                            let _ = events.unbounded_send(ImportEvent::Downloading(p));
                        };
                        match crate::download::download(&url, &into, &cancelled, progress) {
                            Ok(Some(path)) => {
                                let _ = events.unbounded_send(ImportEvent::Downloaded { path: path.clone() });
                                path
                            }
                            Ok(None) => return,
                            Err(err) => {
                                let _ = events.unbounded_send(ImportEvent::Failed(format!("{err:#}")));
                                return;
                            }
                        }
                    }
                };
                let event = match run(&path, codec, playback_rate, &cancelled, &events) {
                    Ok(Some(store)) => ImportEvent::Done { store },
                    Ok(None) => return,
                    Err(err) => ImportEvent::Failed(format!("{err:#}")),
                };
                let _ = events.unbounded_send(event);
            })
            .expect("spawn import thread");
        Import { cancel }
    }
}

fn open_decoder(params: codec::Parameters) -> Result<codec::decoder::Decoder> {
    let mut ctx = codec::context::Context::from_parameters(params)?;
    ctx.set_threading(threading::Config::count(0));
    Ok(ctx.decoder())
}

fn seconds(tb: Rational, ts: i64) -> f64 {
    ts as f64 * tb.0 as f64 / tb.1 as f64
}

/// The file's audio, decoded and resampled to interleaved stereo f32.
struct AudioIn {
    stream: usize,
    time_base: Rational,
    decoder: decoder::Audio,
    rate: u32,
    /// Rebuilt whenever the decoded format changes.
    resampler: Option<resampling::Context>,
}

impl AudioIn {
    fn resample(&mut self, decoded: &mut frame::Audio) -> Result<Vec<f32>> {
        // Containers like MKV don't store a layout; swresample would assume the
        // default one and then reject the frame for not matching it.
        if decoded.channel_layout().is_empty() {
            decoded.set_channel_layout(ChannelLayout::default(decoded.channels() as i32));
        }
        let input = resampling::context::Definition {
            format: decoded.format(),
            channel_layout: decoded.channel_layout(),
            rate: decoded.rate(),
        };
        if self.resampler.as_ref().is_none_or(|r| *r.input() != input) {
            self.resampler = Some(resampling::Context::get(
                input.format,
                input.channel_layout,
                input.rate,
                Sample::F32(sample::Type::Packed),
                ChannelLayout::default(RECORD_CHANNELS as i32),
                self.rate,
            )?);
        }
        // Room for everything this frame can produce, so nothing backs up inside swr.
        let capacity = decoded.samples() * self.rate as usize / input.rate.max(1) as usize + 256;
        let mut out = frame::Audio::new(
            Sample::F32(sample::Type::Packed),
            capacity,
            ChannelLayout::default(RECORD_CHANNELS as i32),
        );
        self.resampler.as_mut().unwrap().run(decoded, &mut out)?;
        let len = out.samples() * RECORD_CHANNELS * 4;
        Ok(out.data(0)[..len].chunks_exact(4).map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]])).collect())
    }
}

/// Feeds decoded frames to the pipeline, stamped on a clock whose origin
/// (`t0`) is the file's first video frame.
struct Feeder {
    video: std::sync::mpsc::SyncSender<TapFrame>,
    audio: Option<std::sync::mpsc::SyncSender<AudioChunk>>,
    t0: Instant,
    /// File time of the first video frame.
    origin: Option<f64>,
    /// Audio decoded before the first video frame (the pipeline would drop it).
    early_audio: Vec<(f64, Vec<f32>)>,
    to_bgra: ToBgra,
    frames: u64,
}

impl Feeder {
    fn at(&self, time: f64) -> Instant {
        let offset = time - self.origin.unwrap_or(time);
        if offset >= 0.0 {
            self.t0 + Duration::from_secs_f64(offset)
        } else {
            self.t0.checked_sub(Duration::from_secs_f64(-offset)).unwrap_or(self.t0)
        }
    }

    /// False once the pipeline has stopped.
    fn video(&mut self, time: f64, decoded: &frame::Video) -> Result<bool> {
        let image = self.to_bgra.convert(decoded)?;
        self.origin.get_or_insert(time);
        if self.video.send(TapFrame { image, at: self.at(time) }).is_err() {
            return Ok(false);
        }
        self.frames += 1;
        for (time, samples) in std::mem::take(&mut self.early_audio) {
            self.audio(time, samples);
        }
        Ok(true)
    }

    fn audio(&mut self, time: f64, samples: Vec<f32>) {
        if self.origin.is_none() {
            self.early_audio.push((time, samples));
        } else if let Some(tx) = &self.audio {
            let _ = tx.send(AudioChunk { samples, at: self.at(time) });
        }
    }
}

fn run(
    path: &Path,
    codec: Codec,
    playback_rate: Option<u32>,
    cancel: &AtomicBool,
    events: &UnboundedSender<ImportEvent>,
) -> Result<Option<Store>> {
    ffmpeg::init().context("initializing FFmpeg")?;
    let mut input = ffmpeg::format::input(path).with_context(|| format!("opening {}", path.display()))?;
    let file_secs = input.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);

    let stream = input.streams().best(media::Type::Video).context("the file has no video")?;
    let (video_stream, video_tb) = (stream.index(), stream.time_base());
    let start_secs = seconds(video_tb, stream.start_time().max(0));
    let fps = [stream.avg_frame_rate(), stream.rate()]
        .into_iter()
        .map(|r| r.0 as f64 / r.1 as f64)
        .find(|fps| fps.is_finite() && *fps > 0.0)
        .context("the file's frame rate is unknown")?;
    let mut video = open_decoder(stream.parameters())?.video().context("opening video decoder")?;

    let mut audio = match input.streams().best(media::Type::Audio) {
        Some(stream) => {
            let decoder = open_decoder(stream.parameters())?.audio().context("opening audio decoder")?;
            Some(AudioIn {
                stream: stream.index(),
                time_base: stream.time_base(),
                rate: playback_rate.unwrap_or(decoder.rate()),
                decoder,
                resampler: None,
            })
        }
        None => None,
    };

    let format = VideoFormat {
        width: video.width(),
        height: video.height(),
        fps,
        pixel: video.format().descriptor().map_or("file", |d| d.name()).into(),
    };
    let (pipeline_tx, mut pipeline_rx) = futures::channel::mpsc::unbounded();
    // The whole file is kept: nothing ages out of an imported buffer.
    let (pipeline, taps) =
        Pipeline::start(codec, format, audio.as_ref().map(|a| a.rate), Duration::MAX, pipeline_tx);
    let store = match futures::executor::block_on(pipeline_rx.next()) {
        Some(PipelineEvent::Ready { store, .. }) => store,
        Some(PipelineEvent::Failed(err) | PipelineEvent::Error(err)) => return Err(anyhow!(err)),
        _ => return Err(anyhow!("the encoder didn't start")),
    };

    let mut feeder = Feeder {
        video: taps.video,
        audio: taps.audio,
        t0: Instant::now(),
        origin: None,
        early_audio: Vec::new(),
        to_bgra: ToBgra::default(),
        frames: 0,
    };
    let mut decoded_video = frame::Video::empty();
    let mut decoded_audio = frame::Audio::empty();
    let mut reported = 0;
    let mut running = true;
    // Packets, then a final pass (None) that drains both decoders.
    let packets = input.packets().map(Some).chain([None]);
    for item in packets {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let is_video = match &item {
            Some((stream, packet)) if stream.index() == video_stream => {
                video.send_packet(packet)?;
                true
            }
            Some((stream, packet)) => match &mut audio {
                Some(a) if stream.index() == a.stream => {
                    a.decoder.send_packet(packet)?;
                    false
                }
                _ => continue,
            },
            None => {
                video.send_eof()?;
                if let Some(a) = &mut audio {
                    a.decoder.send_eof()?;
                }
                true
            }
        };
        if is_video || item.is_none() {
            while running && video.receive_frame(&mut decoded_video).is_ok() {
                let Some(ts) = decoded_video.timestamp() else { continue };
                let time = seconds(video_tb, ts);
                running = feeder.video(time, &decoded_video)?;
                let progress = ((time - start_secs) / file_secs.max(1e-3)).clamp(0.0, 1.0);
                if (progress * 100.0) as u32 > reported {
                    reported = (progress * 100.0) as u32;
                    let _ = events.unbounded_send(ImportEvent::Progress(progress as f32));
                }
            }
        }
        if (!is_video || item.is_none())
            && let Some(a) = &mut audio
        {
            while a.decoder.receive_frame(&mut decoded_audio).is_ok() {
                let Some(ts) = decoded_audio.timestamp() else { continue };
                let samples = a.resample(&mut decoded_audio)?;
                feeder.audio(seconds(a.time_base, ts), samples);
            }
        }
        if !running {
            break;
        }
    }

    let frames = feeder.frames;
    drop(feeder);
    pipeline.join();
    while let Ok(event) = pipeline_rx.try_recv() {
        if let PipelineEvent::Failed(err) | PipelineEvent::Error(err) = event {
            return Err(anyhow!(err));
        }
    }
    if frames == 0 || store.video_range().is_none() {
        return Err(anyhow!("no video frames could be decoded"));
    }
    Ok(Some(store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::clip::save_clip;
    use crate::record::pipeline::tests::synthetic_pipeline;
    use crate::record::player::Player;
    use crate::record::player::tests::next_frame;
    use crate::record::store::StreamKind;
    use crate::video::FrameSlot;

    fn import(path: &Path, codec: Codec, playback_rate: Option<u32>) -> Store {
        import_source(Source::File(path.to_path_buf()), codec, playback_rate)
    }

    fn import_source(source: Source, codec: Codec, playback_rate: Option<u32>) -> Store {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let _import = Import::start(source, codec, playback_rate, tx);
        loop {
            match futures::executor::block_on(rx.next()) {
                Some(ImportEvent::Done { store }) => return store,
                Some(ImportEvent::Failed(err)) => panic!("{err}"),
                Some(_) => {}
                None => panic!("import thread exited"),
            }
        }
    }

    /// Steps through every buffered frame and returns the numbers drawn in them.
    fn buffered_numbers(store: Store) -> Vec<u32> {
        let frames = store.frames_between(f64::MIN, f64::MAX);
        let start = store.video_range().unwrap().0;
        let slot = Arc::new(FrameSlot::default());
        let (tx, mut events) = futures::channel::mpsc::unbounded();
        let player = Player::start(store, slot.clone(), None, tx);
        player.seek(start);
        let mut numbers = vec![next_frame(&mut events, &slot)];
        for _ in 1..frames {
            player.step(1);
            numbers.push(next_frame(&mut events, &slot));
        }
        numbers
    }

    #[test]
    #[ignore = "needs FFmpeg (and ffmpeg on PATH); run with -- --ignored --nocapture"]
    fn imported_files_keep_every_frame_in_order() {
        let fps = 59.94;
        let frame = |n: u32| n as f64 * 1001.0 / 60000.0;
        let dir = std::env::temp_dir().join(format!("frameback-import-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // A numbered H.264 clip with audio, starting mid-GOP (hidden pre-roll)...
        let s = synthetic_pipeline(Codec::H264, fps, true);
        s.feed(0..200, fps);
        let (store, _) = s.finish();
        let clip = dir.join("source.mp4");
        save_clip(&store, Codec::H264, frame(10), frame(100), clip.clone()).unwrap();
        // ...and the same frames re-encoded with B-frames, as most cameras write them.
        let bframes = dir.join("bframes.mp4");
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&clip)
            .args(["-c:v", "libx264", "-bf", "3", "-g", "24", "-c:a", "copy"])
            .arg(&bframes)
            .output()
            .expect("ffmpeg on PATH");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

        for (path, codec) in [(&clip, Codec::Ffv1), (&bframes, Codec::Ffv1), (&bframes, Codec::Hevc)] {
            let store = import(path, codec, None);
            let name = format!("{} as {}", path.file_name().unwrap().to_string_lossy(), codec.label());
            assert!(store.stream_of(StreamKind::Audio).is_some(), "{name}: audio kept");
            let numbers = buffered_numbers(store.clone());
            println!("{name}: {}..={} ({} frames)", numbers[0], numbers.last().unwrap(), numbers.len());
            assert_eq!(numbers, (10..=100).collect::<Vec<_>>(), "{name}");

            // Clipping the imported footage: frames 20..=30 of the original.
            let start = store.video_range().unwrap().0;
            let saved = dir.join(format!("reclip.{}", codec.extension()));
            let frames = save_clip(&store, codec, start + frame(10), start + frame(20), saved.clone()).unwrap();
            assert_eq!(frames, 11, "{name}: clip length");
            // Resampled to a 44.1 kHz output, as when the monitor runs at another rate.
            let reclip = import(&saved, Codec::Ffv1, Some(44_100));
            let audio = reclip.stream_of(StreamKind::Audio).unwrap();
            assert_eq!(reclip.streams()[audio].time_base, Rational(1, 44_100), "{name}: resampled");
            assert_eq!(buffered_numbers(reclip), (20..=30).collect::<Vec<_>>(), "{name}: clip");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "downloads from YouTube with yt-dlp; run with -- --ignored --nocapture"]
    fn imports_from_a_url() {
        let into = std::env::temp_dir().join(format!("frameback-url-test-{}", std::process::id()));
        std::fs::create_dir_all(&into).unwrap();
        let url = "https://www.youtube.com/watch?v=jNQXAC9IVRw".to_string();
        let store = import_source(Source::Url { url, into: into.clone() }, Codec::Hevc, None);
        let frames = store.frames_between(f64::MIN, f64::MAX);
        let (start, end) = store.video_range().unwrap();
        println!("{frames} frames, {:.1} s", end - start);
        // "Me at the zoo" is 19 s long.
        assert!((18.0..20.0).contains(&(end - start)), "{start}..{end}");
        assert!(store.stream_of(StreamKind::Audio).is_some(), "audio kept");
        std::fs::remove_dir_all(&into).ok();
    }
}
