# Frameback

A minimal, low-latency capture-card viewer in the spirit of AmaRecTV, built with Rust and [GPUI](https://www.gpui.rs).

## Run

```sh
cargo run --release
```

Always use `--release` for real use; debug builds add conversion latency.

| Key | Action |
| --- | --- |
| `F` / double-click | Toggle fullscreen |
| `Esc` | Close menu / show hidden overlays / exit fullscreen |
| `M` | Mute audio monitor |
| `I` | Stats overlay (fps, capture→UI latency) |
| `H` | Hide/show everything drawn over the picture (toolbar, timeline, badge, review border, stats) for a clean view while stepping frames |
| `S` | Screenshot to `~/Pictures` |
| `R` | Start/stop recording to `~/Movies` (`~/Videos` on Windows/Linux) |
| `Space` | Pause/play (pausing live enters review on the newest frame) |
| `←` / `→` | Step back/forward by the step size (1, 5, 10, 30 or 60 frames; set on the timeline) |
| `Shift+←` / `Shift+→` | Jump 5 s back/forward |
| `C` | Open/close clip mode |
| `[` / `]` | Mark the clip's first / last frame (clip mode) |
| `Cmd+S` / `Ctrl+S` | Save the marked clip (clip mode; the whole buffer if nothing is marked) |
| `Cmd+C` / `Ctrl+C` | Copy the marked clip to the clipboard as a file (clip mode) |
| `Cmd+Shift+C` / `Ctrl+Shift+C` | Copy the frame on screen to the clipboard as an image (clip mode) |
| `Cmd+O` / `Ctrl+O` | Import a video file to review and clip |
| `Cmd+Shift+O` / `Ctrl+Shift+O` | Import a video from a URL |
| `L` / `End` | Back to live (closes an imported file, returning to where you were) |

Controls hide after 2.5 s without mouse movement. The toolbar holds recording, the capture device, format and audio; everything else is in its menus: **File** (import a file or URL, screenshot), **View** (stats, hide overlays, fullscreen) and **Settings** (codec, rewind buffer length).

## How latency is kept low

- **Video (macOS):** a native AVFoundation backend asks devices for BGRA, so colour conversion happens in hardware (`420v`, `BGRA`, …). Packed 4:2:2 (`yuvs`, `2vuy`) is converted on the CPU instead, because some UVC drivers misreport its row stride (an Elgato HD60 S+ at 720×480 shears otherwise); frameback detects the real stride per frame. Frames go straight from the capture callback into a single slot, and AVFoundation discards late frames instead of queueing them.
- **Video (Windows/Linux):** nokhwa (MediaFoundation/V4L2); YUYV is converted on the CPU in ~1 ms at 1080p and written into the same single slot.
- **Display:** the UI is woken once and draws only the newest frame, so a slow UI frame drops frames instead of adding delay. Replaced textures are freed from the GPU atlas one render later.
- **Audio:** input → lock-free ring buffer → default output, with ~5 ms callbacks where the device allows. If clock drift makes the buffer grow, old samples are dropped so delay can't accumulate.
- **Formats:** defaults to the widest mode, then the tallest, then the highest frame rate. Fractional rates like 59.94 are kept exact. Pick another mode from the format menu.

Audio monitoring starts automatically only when an input device's name matches the video device (typical for capture cards), so it never starts looping your microphone.

## DVR (rewind the live feed)

While a device is open, everything is encoded into a rewind buffer on disk (1 minute by default, up to 60 under **Settings**; old footage is deleted as it ages out).

frameback has two modes:

- **LIVE** (red badge): the preview is live and the buffer is filling.
- **REVIEW** (amber badge and frame; frame-step controls appear on the timeline): entered by pausing, stepping back, jumping or clicking/dragging the timeline. The buffer is **frozen**: nothing is added and nothing ages out, so what you're reviewing can't be overwritten. Drag the timeline, step frames, or play at 1× with audio. Actions that would clear the buffer (changing device, format, audio, codec or buffer length) and starting a recording are refused until you go back LIVE (`L`). A recording already running continues. Reaching the end of the buffer stops there; going live is always explicit.

### Saving clips

Clip editing is its own mode (green **CLIP** badge): click **Clip** on the timeline or press `C` (from live, this also enters review). `Esc` or **Clip** again closes it and drops the marks. Step to a frame and press `[` (or **[ Start**) to mark the clip's first frame, then `]` (or **End ]**) for the last. The marked range is highlighted on the timeline with its length and frame count. **Save clip** (`Cmd+S`) writes it to `~/Movies` as `frameback-clip-….` without re-encoding: clips start and end on exactly the marked frames for every codec (HEVC/H.264 clips carry the frames back to the previous keyframe as hidden pre-roll, which players skip).

**Copy clip** (`Cmd+C`) puts the same file on the clipboard, ready to paste into Finder, Slack, Discord or Messages; copied clips live in a temp folder and are deleted after a day. **Copy frame** (`Cmd+Shift+C`) copies the frame on screen as a PNG. Copying clips as files is macOS-only for now; elsewhere the clip's file path is copied instead.

### Importing clips

**File → Import file…** (`Cmd+O`) opens an existing video file in place of the live feed, so it can be stepped, scrubbed and clipped the same way. The file is converted into a buffer using the selected codec. This means any format FFmpeg can read works, B-frames and odd keyframe intervals included, and stepping stays frame-exact. Pick **Lossless (FFV1)** first if every pixel matters for your analysis; the lossy codecs re-encode the footage. Long or high-resolution files take a while to convert and need disk space for the buffer (the **IMPORT** badge shows progress; `L` cancels). Once imported, the whole file is kept, regardless of the DVR length. Variable-frame-rate files are placed on a constant-rate grid at their average rate.

**File → Import from URL…** (`Cmd+Shift+O`) does the same for a video on YouTube or any other site [yt-dlp supports](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md). The box is filled in from the clipboard when it holds a link. The video is downloaded into `~/Movies` (named after its title) and then imported; partial downloads stay in a temp folder and are removed if you cancel (`L`) or the download fails. Release builds include yt-dlp and the `ffmpeg` command it needs to merge separate video and audio streams. Builds run with `cargo run` use the `yt-dlp` and `ffmpeg` on your PATH (`brew install yt-dlp ffmpeg`).

Importing never discards the live rewind buffer: it's frozen while the file is open (as in review), and a recording already running carries on. `L` closes the file and returns to where you were: the same frame, clip marks and clip mode if you were reviewing, or live otherwise. Importing another file while one is open replaces it and still returns to the same place. Audio plays back through the selected audio input's output, so a file's sound is only heard when an audio input is selected. Clips saved from an imported file include its audio either way.

Seeking is frame-exact for every codec. The buffer uses the selected codec, so its size depends on it: lossless FFV1 at 720×480 is about 10 MB/s, while HEVC/H.264 are a few hundred KB/s at the same resolution.

## Recording

Pick a codec under **Settings**; the fastest available encoder is used (hardware first):

| Codec | File | Notes |
| --- | --- | --- |
| Lossless (FFV1) | .mkv | Lossless RGB, every frame a keyframe |
| ProRes 422 HQ | .mov | Editor-friendly, every frame a keyframe |
| HEVC — default | .mp4 | Hardware-encoded where available, small files, keyframe every 0.5 s |
| H.264 | .mp4 | Same, for players without HEVC support |

Recordings include the capture card's audio (PCM for FFV1/ProRes, AAC for HEVC/H.264) when an audio input is selected. A recording starts at the frame where you pressed `R` (or the keyframe just before it, ≤ 0.5 s, for HEVC/H.264), using what's already in the DVR buffer, so audio and video start together.

Recording runs on its own thread and never delays the live preview; if the encoder can't keep up, frames are dropped from the file (reported when it's saved) rather than from the screen. Files are written at the rate the device actually delivers, which can be lower than advertised.

## Releases and updates

Download the latest build from GitHub Releases (macOS Apple Silicon `.zip`, Windows `.zip`, Linux `.tar.gz`); each bundles its own FFmpeg. On first launch, macOS blocks the app because it isn't notarized (it's self-signed): open **System Settings → Privacy & Security** and click **Open Anyway** next to the Frameback message. Later updates install without asking again.

On launch, Frameback checks for a newer release. **Update to vX** downloads it and checks it against the release's `SHA256SUMS`; **Restart to update** quits, swaps in the new version and reopens. It never updates while recording, and builds run from `cargo run` never update. Pre-releases (tags like `v1.2.0-rc.1`) aren't offered.

To publish a release: bump `version` in `Cargo.toml`, commit, and push a matching tag (`git tag v0.2.0 && git push origin v0.2.0`). The Release workflow tests, builds and packages all three platforms (`scripts/package-*.{sh,ps1}`) and publishes them with checksums. Its comments explain the macOS signing certificate secrets. Bump the bundled yt-dlp first (see `packaging/yt-dlp.sha256`): sites change often, and an old yt-dlp is the usual reason a URL stops downloading.

## Platform prerequisites

- **All platforms:** FFmpeg 9 shared libraries and headers (`ffmpeg-next` links against them). Note that Homebrew's and most distro FFmpeg builds are GPL, which affects how frameback binaries built against them can be distributed.

- **macOS:** `brew install ffmpeg`, Xcode command-line tools. Shaders are compiled at startup (`runtime_shaders`), so the separate Metal Toolchain isn't needed. The terminal or app needs camera permission.
- **Linux:** FFmpeg dev packages (`libavcodec-dev` etc.), `libclang` (for bindgen), `libasound2-dev`, `libxkbcommon-dev`, Wayland/X11 dev headers, and a Vulkan driver (GPUI requirements). Not yet tested here.
- **Windows:** MSVC toolchain, a shared FFmpeg build with `FFMPEG_DIR` pointing at it. Not yet tested here.

## Tests

```sh
cargo test                                                  # unit tests
cargo test --release every_device -- --ignored --nocapture  # opens each real device, grabs a frame
cargo test --release -- --ignored every_codec_records seeks_and_steps plays_forward frozen_buffer playback_cuts clips_start imported_files  # recording, A/V sync, frame-exact seeking, review freeze, clips, imports
FRAMEBACK_TEST_DEVICE=HD60 cargo test --release records_a_device_with_audio -- --ignored --nocapture
cargo test --release -- --ignored downloads_a_video cancelling_stops imports_from_a_url  # URL import (network, yt-dlp + ffmpeg on PATH)
```

## Not yet implemented

- Persisting the selected devices/format between launches.

## License

GPL-3.0-or-later. Release builds bundle FFmpeg built with GPL components (x264/x265), and [yt-dlp](https://github.com/yt-dlp/yt-dlp) (Unlicense). The text field (`src/text_input.rs`) is adapted from GPUI's input example (Apache-2.0).
