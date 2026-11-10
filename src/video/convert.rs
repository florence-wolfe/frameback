//! CPU pixel conversion for formats the backend can't (or mustn't) convert itself.

/// Fixed-point (16.16) YCbCr → RGB coefficients for limited-range video.
struct Coeffs {
    y: i32,
    rv: i32,
    gu: i32,
    gv: i32,
    bu: i32,
}

const BT601: Coeffs = Coeffs { y: 76284, rv: 104595, gu: 25690, gv: 53281, bu: 132186 };
const BT709: Coeffs = Coeffs { y: 76284, rv: 117489, gu: 13975, gv: 34925, bu: 138438 };

/// Byte order of a packed 4:2:2 macropixel (two pixels in four bytes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Packed422 {
    /// Y0 Cb Y1 Cr — YUY2, macOS `yuvs`.
    Yuyv,
    /// Cb Y0 Cr Y1 — macOS `2vuy`.
    Uyvy,
}

impl Packed422 {
    /// Offsets of (Y0, Cb, Y1, Cr) within a macropixel.
    fn offsets(self) -> [usize; 4] {
        match self {
            Packed422::Yuyv => [0, 1, 2, 3],
            Packed422::Uyvy => [1, 0, 3, 2],
        }
    }
}

/// Converts packed 4:2:2 YCbCr to BGRA. HD sources use BT.709, SD uses BT.601.
/// `stride` is the distance in bytes between the starts of consecutive rows.
pub fn packed422_to_bgra(
    src: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    order: Packed422,
    dst: &mut [u8],
) {
    let (w, h) = (width as usize, height as usize);
    let c = if height >= 720 { &BT709 } else { &BT601 };
    let [y0, cb, y1, cr] = order.offsets();

    for (row, out) in dst.chunks_exact_mut(w * 4).take(h).enumerate() {
        let Some(line) = src.get(row * stride..row * stride + w * 2) else {
            break;
        };
        for (mp, px) in line.chunks_exact(4).zip(out.chunks_exact_mut(8)) {
            let u = mp[cb] as i32 - 128;
            let v = mp[cr] as i32 - 128;
            let r = c.rv * v;
            let g = -c.gu * u - c.gv * v;
            let b = c.bu * u;
            for (y, p) in [mp[y0], mp[y1]].into_iter().zip(px.chunks_exact_mut(4)) {
                let y = (y as i32 - 16) * c.y + (1 << 15);
                p[0] = clamp(y + b);
                p[1] = clamp(y + g);
                p[2] = clamp(y + r);
                p[3] = 255;
            }
        }
    }
}

/// Picks the real row stride of a packed 4:2:2 buffer.
///
/// Some macOS UVC drivers (seen with an Elgato HD60 S+ at 720×480) report a
/// 64-byte-aligned `bytesPerRow` but write rows tightly packed, which shears
/// the image. Natural images change little from one row to the next, so the
/// candidate whose neighbouring rows differ least is the real one. Ties
/// (e.g. a black frame) go to the declared stride.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn detect_packed_stride(src: &[u8], declared: usize, width: u32, height: u32) -> usize {
    let packed = width as usize * 2;
    if declared <= packed {
        return declared;
    }
    let row_difference = |stride: usize| -> u64 {
        let rows = (height as usize).saturating_sub(1);
        let step = (rows / 32).max(1);
        (0..rows)
            .step_by(step)
            .filter_map(|r| {
                let a = src.get(r * stride..r * stride + packed)?;
                let b = src.get((r + 1) * stride..(r + 1) * stride + packed)?;
                // Compare luma-carrying bytes of every other macropixel; plenty of signal, cheap.
                Some(a.iter().zip(b).step_by(8).map(|(x, y)| x.abs_diff(*y) as u64).sum::<u64>())
            })
            .sum()
    };
    if row_difference(packed) < row_difference(declared) { packed } else { declared }
}

/// Whether frames this size carry a 240p/288p picture (one field per frame,
/// e.g. a retro console through a pass-through scaler at 720×240) that is meant
/// to fill a 4:3 screen with every line shown twice.
pub fn line_doubled(width: u32, height: u32) -> bool {
    height <= 288 && width >= height * 2
}

/// Repeats every row of a BGRA frame that `line_doubled` matches, the way a TV
/// or line doubler shows it; plain repetition keeps pixel edges sharp.
/// Returns the frame and its height.
pub fn double_lines(bgra: Vec<u8>, width: u32, height: u32) -> (Vec<u8>, u32) {
    if !line_doubled(width, height) {
        return (bgra, height);
    }
    let mut doubled = Vec::with_capacity(bgra.len() * 2);
    for row in bgra.chunks_exact(width as usize * 4) {
        doubled.extend_from_slice(row);
        doubled.extend_from_slice(row);
    }
    (doubled, height * 2)
}

#[inline(always)]
fn clamp(v: i32) -> u8 {
    (v >> 16).clamp(0, 255) as u8
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn rgba_to_bgra(src: &[u8], dst: &mut [u8]) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d.copy_from_slice(&[s[2], s[1], s[0], 255]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn convert(yuyv: &[u8], width: u32, height: u32) -> Vec<u8> {
        convert_with(yuyv, yuyv.len() / height as usize, width, height, Packed422::Yuyv)
    }

    fn convert_with(src: &[u8], stride: usize, width: u32, height: u32, order: Packed422) -> Vec<u8> {
        let mut out = vec![0; (width * height * 4) as usize];
        packed422_to_bgra(src, stride, width, height, order, &mut out);
        out
    }

    #[test]
    fn limited_range_black_and_white() {
        assert_eq!(convert(&[16, 128, 16, 128], 2, 1), [0, 0, 0, 255, 0, 0, 0, 255]);
        assert_eq!(convert(&[235, 128, 235, 128], 2, 1), [255; 8]);
    }

    #[test]
    fn primaries_land_in_the_right_channels() {
        // BT.601 pure red: Y=81 U=90 V=240. Output is BGRA.
        let px = convert(&[81, 90, 81, 240], 2, 1);
        assert!(px[2] >= 250 && px[1] <= 5 && px[0] <= 5, "{px:?}");
        // BT.601 pure blue: Y=41 U=240 V=110.
        let px = convert(&[41, 240, 41, 110], 2, 1);
        assert!(px[0] >= 250 && px[1] <= 5 && px[2] <= 5, "{px:?}");
    }

    #[test]
    fn padded_rows_are_skipped() {
        // 2×2 frame, each row padded with 4 junk bytes.
        let src = [16, 128, 16, 128, 9, 9, 9, 9, 235, 128, 235, 128, 9, 9, 9, 9];
        let px = convert(&src, 2, 2);
        assert_eq!(&px[..8], &[0, 0, 0, 255, 0, 0, 0, 255]);
        assert_eq!(&px[8..], &[255; 8]);
    }

    #[test]
    fn uyvy_matches_yuyv() {
        // BT.601 pure red as YUYV and as UYVY.
        let yuyv = convert_with(&[81, 90, 81, 240], 4, 2, 1, Packed422::Yuyv);
        let uyvy = convert_with(&[90, 81, 240, 81], 4, 2, 1, Packed422::Uyvy);
        assert_eq!(yuyv, uyvy);
    }

    /// A 720×480 YUYV frame with vertical stripes (the kind of detail that shears),
    /// laid out with the given stride. Padding bytes are left at zero.
    fn striped_frame(stride: usize, buffer_len: usize) -> Vec<u8> {
        let (w, h) = (720, 480);
        let mut buf = vec![0u8; buffer_len];
        for row in 0..h {
            for x in 0..w {
                let luma = if (x / 20 + row / 60) % 2 == 0 { 200 } else { 40 };
                buf[row * stride + x * 2] = luma;
                buf[row * stride + x * 2 + 1] = 128;
            }
        }
        buf
    }

    #[test]
    fn detects_packed_rows_in_padded_buffer() {
        // What the HD60 S+ delivers: bytesPerRow 1472, rows actually 1440 apart.
        let buf = striped_frame(1440, 1472 * 481);
        assert_eq!(detect_packed_stride(&buf, 1472, 720, 480), 1440);
    }

    #[test]
    fn keeps_declared_stride_when_it_is_right() {
        let buf = striped_frame(1472, 1472 * 481);
        assert_eq!(detect_packed_stride(&buf, 1472, 720, 480), 1472);
    }

    #[test]
    fn black_frame_keeps_declared_stride() {
        let buf = vec![0u8; 1472 * 481];
        assert_eq!(detect_packed_stride(&buf, 1472, 720, 480), 1472);
    }

    #[test]
    fn low_line_count_frames_are_line_doubled() {
        // 2×1 frame standing in for 720×240: wider than twice its height.
        let row = [1, 2, 3, 255, 4, 5, 6, 255];
        let (px, height) = double_lines(row.to_vec(), 2, 1);
        assert_eq!(height, 2);
        assert_eq!(&px[..8], &row);
        assert_eq!(&px[8..], &row);
    }

    #[test]
    fn which_frames_get_line_doubled() {
        assert!(line_doubled(720, 240), "NTSC 240p");
        assert!(line_doubled(720, 288), "PAL 288p");
        assert!(!line_doubled(720, 480));
        assert!(!line_doubled(1920, 1080));
        assert!(!line_doubled(320, 240), "4:3 webcam mode");
    }

    #[test]
    #[ignore = "benchmark; run with --release -- --ignored --nocapture"]
    fn bench_1080p() {
        let src = vec![128u8; 1920 * 1080 * 2];
        let mut dst = vec![0u8; 1920 * 1080 * 4];
        let start = Instant::now();
        for _ in 0..100 {
            packed422_to_bgra(&src, 3840, 1920, 1080, Packed422::Yuyv, &mut dst);
        }
        println!("1080p YUYV→BGRA: {:?}/frame", start.elapsed() / 100);
    }
}
