#!/bin/bash
# Builds dist/Frameback/: the release binary plus FFmpeg's shared libraries in
# lib/, found through an $ORIGIN rpath so no system FFmpeg is needed. yt-dlp
# and the ffmpeg command it merges downloads with go beside the binary; the
# app points ffmpeg at lib/ through LD_LIBRARY_PATH.
# Expects FFMPEG_DIR to point at an FFmpeg 9 build (CI uses BtbN's).
set -euo pipefail
cd "$(dirname "$0")/.."
: "${FFMPEG_DIR:?set FFMPEG_DIR to an FFmpeg 9 build}"

# RPATH rather than the default RUNPATH: only RPATH also applies to the
# libraries FFmpeg's own libraries load (e.g. libavdevice -> libavfilter).
RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,--disable-new-dtags,-rpath,\$ORIGIN/lib" cargo build --release --locked

out=dist/Frameback
rm -rf "$out"
mkdir -p "$out/lib"
cp target/release/frameback "$out/frameback"
cp packaging/icon.png "$out/frameback.png"
cp -a "$FFMPEG_DIR"/lib/*.so* "$out/lib/"
cp "$FFMPEG_DIR/bin/ffmpeg" "$out/ffmpeg"

# yt-dlp, pinned and checked against packaging/yt-dlp.sha256.
yt_dlp_version=$(awk '$1 == "version" { print $2 }' packaging/yt-dlp.sha256)
curl -fsSL -o "$out/yt-dlp" \
    "https://github.com/yt-dlp/yt-dlp/releases/download/$yt_dlp_version/yt-dlp_linux"
echo "$(awk '$2 == "yt-dlp_linux" { print $1 }' packaging/yt-dlp.sha256)  $out/yt-dlp" | sha256sum -c -
chmod +x "$out/yt-dlp"

# Every FFmpeg library must resolve from the bundle.
missing=$(LD_LIBRARY_PATH= ldd "$out/frameback" | grep -E 'lib(av|sw)[a-z]*\.so' | grep -v "$out/lib" || true)
if [ -n "$missing" ]; then
    echo "FFmpeg libraries not bundled:"; echo "$missing"; exit 1
fi
missing=$(LD_LIBRARY_PATH="$out/lib" ldd "$out/ffmpeg" | grep -E 'lib(av|sw|postproc)[a-z]*\.so' | grep -v "$out/lib" || true)
if [ -n "$missing" ]; then
    echo "Libraries the bundled ffmpeg can't find in lib/:"; echo "$missing"; exit 1
fi
echo "Built $out ($(du -sh "$out" | cut -f1))"
