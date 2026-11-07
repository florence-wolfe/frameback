//! Recording codecs offered in the UI, and how each maps onto FFmpeg.

use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// Lossless, every frame a keyframe, available in every FFmpeg build.
    Ffv1,
    ProRes,
    Hevc,
    H264,
}

impl Codec {
    pub const ALL: [Codec; 4] = [Codec::Ffv1, Codec::ProRes, Codec::Hevc, Codec::H264];

    pub fn label(self) -> &'static str {
        match self {
            Codec::Ffv1 => "Lossless (FFV1)",
            Codec::ProRes => "ProRes 422 HQ",
            Codec::Hevc => "HEVC",
            Codec::H264 => "H.264",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Codec::Ffv1 => "mkv",
            Codec::ProRes => "mov",
            Codec::Hevc | Codec::H264 => "mp4",
        }
    }

    pub fn container(self) -> &'static str {
        match self {
            Codec::Ffv1 => "matroska",
            Codec::ProRes => "mov",
            Codec::Hevc | Codec::H264 => "mp4",
        }
    }

    /// FFmpeg encoder names, hardware first. The first one that opens wins.
    pub fn encoders(self) -> &'static [&'static str] {
        match self {
            Codec::Ffv1 => &["ffv1"],
            Codec::ProRes => &["prores_videotoolbox", "prores_ks"],
            Codec::Hevc => &[
                "hevc_videotoolbox",
                "hevc_nvenc",
                "hevc_qsv",
                "hevc_amf",
                "hevc_mf",
                "libx265",
            ],
            Codec::H264 => &[
                "h264_videotoolbox",
                "h264_nvenc",
                "h264_qsv",
                "h264_amf",
                "h264_mf",
                "libx264",
            ],
        }
    }

    /// Pixel formats in order of preference; the first one the encoder supports is used.
    pub fn preferred_pixels(self) -> &'static [Pixel] {
        match self {
            // RGB keeps FFV1 truly lossless relative to what's on screen.
            Codec::Ffv1 => &[Pixel::BGRZ, Pixel::YUV444P, Pixel::YUV422P],
            Codec::ProRes => &[Pixel::YUV422P10LE, Pixel::P210LE, Pixel::NV16],
            Codec::Hevc | Codec::H264 => &[Pixel::NV12, Pixel::YUV420P],
        }
    }

    /// Keyframe interval in frames. Intra-only codecs make every frame seekable.
    pub fn gop(self, fps: f64) -> u32 {
        match self {
            Codec::Ffv1 | Codec::ProRes => 1,
            // Every 0.5 s keeps DVR scrubbing quick.
            Codec::Hevc | Codec::H264 => (fps / 2.0).round().max(1.0) as u32,
        }
    }

    /// Target bit rate for the lossy codecs: ~0.3 bits per pixel per frame
    /// (≈37 Mbit/s at 1080p60), generous enough for fast game footage.
    pub fn bit_rate(self, width: u32, height: u32, fps: f64) -> Option<usize> {
        match self {
            Codec::Hevc | Codec::H264 => {
                Some((width as f64 * height as f64 * fps * 0.3) as usize)
            }
            _ => None,
        }
    }

    /// Encoder-specific options for a given encoder name.
    pub fn options(self, encoder: &str) -> Vec<(&'static str, &'static str)> {
        match encoder {
            "ffv1" => vec![("level", "3"), ("slices", "16"), ("slicecrc", "1")],
            "prores_ks" => vec![("profile", "3")],
            "prores_videotoolbox" => vec![("profile", "hq")],
            "h264_videotoolbox" | "hevc_videotoolbox" => vec![("realtime", "1")],
            "libx264" | "libx265" => vec![("preset", "veryfast"), ("crf", "18")],
            _ => Vec::new(),
        }
    }

    /// Audio encoder: uncompressed for the lossless/editing codecs, AAC for MP4.
    pub fn audio_encoder(self) -> &'static str {
        match self {
            Codec::Ffv1 | Codec::ProRes => "pcm_s16le",
            Codec::Hevc | Codec::H264 => "aac",
        }
    }

    /// Muxer options. MP4/MOV are written fragmented so a crash doesn't lose the file.
    pub fn muxer_options(self) -> Vec<(&'static str, &'static str)> {
        match self {
            Codec::Ffv1 => Vec::new(),
            _ => vec![
                ("movflags", "frag_keyframe+empty_moov+default_base_moof"),
                ("min_frag_duration", "1000000"),
            ],
        }
    }
}

/// Exact frame period as a rational, recognising NTSC rates (59.94 → 1001/60000).
pub fn time_base(fps: f64) -> ffmpeg::Rational {
    let ntsc = (fps * 1.001).round();
    if (ntsc / 1.001 - fps).abs() < 0.005 && (ntsc - fps).abs() > 0.005 {
        ffmpeg::Rational(1001, (ntsc as i32) * 1000)
    } else {
        ffmpeg::Rational(1, fps.round().max(1.0) as i32)
    }
}

/// The frame rate to record at: the advertised rate divided by the whole
/// number that best matches what's actually being delivered.
pub fn effective_fps(advertised: f64, measured: Option<f64>) -> f64 {
    let Some(measured) = measured.filter(|m| *m > 0.0) else {
        return advertised;
    };
    let divisor = (advertised / measured).round().clamp(1.0, 8.0);
    advertised / divisor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_rate_follows_delivery() {
        let ntsc = 59.940179700658746;
        assert_eq!(effective_fps(ntsc, None), ntsc);
        assert_eq!(effective_fps(ntsc, Some(59.1)), ntsc, "measurement noise");
        assert_eq!(effective_fps(ntsc, Some(29.8)), ntsc / 2.0);
        assert_eq!(time_base(effective_fps(ntsc, Some(29.97))), ffmpeg::Rational(1001, 30000));
        assert_eq!(effective_fps(60.0, Some(61.0)), 60.0);
    }

    #[test]
    fn ntsc_rates_get_exact_time_bases() {
        assert_eq!(time_base(59.940179700658746), ffmpeg::Rational(1001, 60000));
        assert_eq!(time_base(29.97), ffmpeg::Rational(1001, 30000));
        assert_eq!(time_base(60.0), ffmpeg::Rational(1, 60));
        assert_eq!(time_base(30.0), ffmpeg::Rational(1, 30));
        assert_eq!(time_base(50.0), ffmpeg::Rational(1, 50));
    }

    #[test]
    fn lossy_codecs_get_half_second_gops() {
        assert_eq!(Codec::H264.gop(59.94), 30);
        assert_eq!(Codec::Ffv1.gop(59.94), 1);
    }
}
