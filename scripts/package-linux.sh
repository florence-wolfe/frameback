#!/bin/bash
# Builds dist/Frameback/: the release binary plus FFmpeg's shared libraries in
# lib/, found through an $ORIGIN rpath so no system FFmpeg is needed.
# Expects FFMPEG_DIR to point at an FFmpeg 9 build (CI uses BtbN's).
set -euo pipefail
cd "$(dirname "$0")/.."
: "${FFMPEG_DIR:?set FFMPEG_DIR to an FFmpeg 9 build}"

RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-rpath,\$ORIGIN/lib" cargo build --release --locked

out=dist/Frameback
rm -rf "$out"
mkdir -p "$out/lib"
cp target/release/frameback "$out/frameback"
cp packaging/icon.png "$out/frameback.png"
cp -a "$FFMPEG_DIR"/lib/*.so* "$out/lib/"

# Every FFmpeg library must resolve from the bundle.
missing=$(LD_LIBRARY_PATH= ldd "$out/frameback" | grep -E 'lib(av|sw)[a-z]*\.so' | grep -v "$out/lib" || true)
if [ -n "$missing" ]; then
    echo "FFmpeg libraries not bundled:"; echo "$missing"; exit 1
fi
echo "Built $out ($(du -sh "$out" | cut -f1))"
