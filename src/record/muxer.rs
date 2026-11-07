//! Writes encoded packets (the same ones the DVR buffer keeps) to a file.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::{Packet, Rational, codec, encoder, format};

use super::codec::Codec;
use super::store::{StreamDesc, StreamKind};

pub struct Muxer {
    octx: format::context::Output,
    path: PathBuf,
    file_time_bases: Vec<Rational>,
    /// Session time that becomes 0 in the file.
    start: Option<f64>,
    /// Video from this keyframe up to `start` is written with negative
    /// timestamps: needed to decode `start`, hidden by players (edit list).
    preroll_from: Option<f64>,
    video_frames: u64,
}

impl Muxer {
    /// `fragmented` trades a little overhead for files that survive a crash;
    /// recordings want it, clips saved in one go don't.
    pub fn create(
        path: PathBuf,
        codec: Codec,
        streams: &[StreamDesc],
        fragmented: bool,
    ) -> Result<Muxer> {
        let mut octx = format::output_as(&path, codec.container())
            .with_context(|| format!("creating {}", path.display()))?;
        for desc in streams {
            let mut stream = octx.add_stream(encoder::find(codec::Id::None))?;
            stream.set_parameters(desc.params.get());
            stream.set_time_base(desc.time_base);
            // Let the muxer pick its own tag for the codec.
            unsafe { (*stream.parameters().as_mut_ptr()).codec_tag = 0 };
        }
        let mut options = ffmpeg::Dictionary::new();
        if fragmented {
            for (key, value) in codec.muxer_options() {
                options.set(key, value);
            }
        }
        octx.write_header_with(options).context("writing file header")?;
        let file_time_bases = octx.streams().map(|s| s.time_base()).collect();
        Ok(Muxer { octx, path, file_time_bases, start: None, preroll_from: None, video_frames: 0 })
    }

    /// Sets where the file begins (session seconds). Must be a video keyframe.
    pub fn set_start(&mut self, start: f64) {
        self.start = Some(start);
    }

    /// Starts the file at any frame: video from `keyframe` (at or before
    /// `start`) is kept as hidden pre-roll so `start` can be decoded.
    pub fn set_start_with_preroll(&mut self, start: f64, keyframe: f64) {
        self.start = Some(start);
        self.preroll_from = Some(keyframe);
    }

    pub fn started(&self) -> bool {
        self.start.is_some()
    }

    /// Writes a packet, rebased so the file starts at 0. Packets before the
    /// start (or before one is set) are skipped.
    pub fn write(
        &mut self,
        streams: &[StreamDesc],
        stream: usize,
        pts: i64,
        duration: i64,
        key: bool,
        data: &[u8],
    ) -> Result<()> {
        let desc = &streams[stream];
        let Some(start) = self.start else { return Ok(()) };
        let rebased = pts - desc.pts(start);
        let preroll = desc.kind == StreamKind::Video
            && self.preroll_from.is_some_and(|from| desc.seconds(pts) >= from - 1e-6);
        if rebased < 0 && !preroll {
            return Ok(());
        }
        let mut packet = Packet::copy(data);
        packet.set_stream(stream);
        packet.set_pts(Some(rebased));
        packet.set_dts(Some(rebased));
        packet.set_duration(duration);
        if key {
            packet.set_flags(codec::packet::Flags::KEY);
        }
        packet.rescale_ts(desc.time_base, self.file_time_bases[stream]);
        packet.write_interleaved(&mut self.octx).context("writing to recording")?;
        if desc.kind == StreamKind::Video && rebased >= 0 {
            self.video_frames += 1;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(PathBuf, u64)> {
        self.octx.write_trailer().context("finalizing recording")?;
        Ok((self.path, self.video_frames))
    }
}
