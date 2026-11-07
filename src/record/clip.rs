//! Saving part of the DVR buffer as a file, without re-encoding.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use ffmpeg_next as ffmpeg;

use super::codec::Codec;
use super::muxer::Muxer;
use super::store::{Store, StreamKind};

/// Writes frames `from..=to` (session seconds of the first and last frame)
/// to `path`. Packets are copied as-is; for codecs with keyframe intervals the
/// frames between the preceding keyframe and `from` are stored as hidden
/// pre-roll, so the clip still starts on exactly `from`. Returns the number of
/// frames in the clip.
pub fn save_clip(store: &Store, codec: Codec, from: f64, to: f64, path: PathBuf) -> Result<u64> {
    ffmpeg::init().context("initializing FFmpeg")?;
    let video = store.stream_of(StreamKind::Video).context("no video in buffer")?;
    let streams = store.streams();
    let desc = &streams[video];
    let (from_pts, to_pts) = (desc.pts(from), desc.pts(to));
    let keyframe = store
        .keyframe_at_or_before(video, from_pts)
        .context("the clip's start is no longer buffered")?;
    let end_time = to + desc.seconds(1); // the last frame lasts one tick

    // Every packet to write, in time order so the muxer interleaves cheaply.
    let mut packets: Vec<(f64, usize, _)> = Vec::new();
    for (stream, d) in streams.iter().enumerate() {
        let (lo, hi) = if stream == video {
            (keyframe.pts, to_pts + 1)
        } else {
            (d.pts(from), d.pts(end_time))
        };
        packets.extend(store.range(stream, lo, hi).into_iter().map(|e| (d.seconds(e.pts), stream, e)));
    }
    packets.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut muxer = Muxer::create(path, codec, streams, false)?;
    muxer.set_start_with_preroll(from, desc.seconds(keyframe.pts));
    let mut reader = store.reader();
    for (_, stream, entry) in packets {
        let data = reader.read(&entry)?;
        muxer.write(streams, stream, entry.pts, entry.duration, entry.key, &data)?;
    }
    let (_, frames) = muxer.finish()?;
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::pipeline::tests::{read_number, synthetic_pipeline};

    /// Decodes a file the way a player would (edit lists applied) and returns
    /// the numbers drawn in its frames.
    fn decoded_numbers(path: &std::path::Path) -> Vec<u32> {
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-f", "rawvideo", "-pix_fmt", "bgra", "-"])
            .output()
            .expect("ffmpeg on PATH");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout.chunks_exact(720 * 480 * 4).map(|f| read_number(f, 720)).collect()
    }

    #[test]
    #[ignore = "needs FFmpeg; run with -- --ignored --nocapture"]
    fn clips_start_and_end_on_the_chosen_frames() {
        let fps = 59.94;
        let frame = |n: u32| n as f64 * 1001.0 / 60000.0;
        let dir = std::env::temp_dir().join(format!("frameback-clip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for codec in Codec::ALL {
            let s = synthetic_pipeline(codec, fps, true);
            s.feed(0..300, fps);
            let (store, _) = s.finish();
            // 37 is mid-GOP for HEVC/H.264 (keyframes every 30 frames).
            let path = dir.join(format!("clip.{}", codec.extension()));
            let frames = save_clip(&store, codec, frame(37), frame(100), path.clone()).unwrap();
            let numbers = decoded_numbers(&path);
            println!("{}: {frames} frames, decoded {}..={} ({} frames)", codec.label(),
                numbers[0], numbers.last().unwrap(), numbers.len());
            assert_eq!(frames, 64, "{}", codec.label());
            assert_eq!(numbers.first(), Some(&37), "{}: first frame", codec.label());
            assert_eq!(numbers.last(), Some(&100), "{}: last frame", codec.label());
            assert_eq!(numbers.len(), 64, "{}: frame count", codec.label());
            assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1), "{}: contiguous", codec.label());
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
