#!/bin/bash
# Builds dist/Frameback.app: the release binary, Info.plist, and FFmpeg's
# libraries (with everything they depend on) bundled in Contents/Frameworks,
# so the app runs without Homebrew. Signs with $FRAMEBACK_SIGN_IDENTITY when
# set (CI), ad hoc otherwise.
#
# macOS keeps the Camera permission across updates only while every release
# is signed with the same certificate; an ad hoc signature changes per build.
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')
cargo build --release --locked

app=dist/Frameback.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources"
cp target/release/frameback "$app/Contents/MacOS/frameback"
cp packaging/Frameback.icns "$app/Contents/Resources/Frameback.icns"
sed "s/__VERSION__/$version/g" packaging/Info.plist > "$app/Contents/Info.plist"

frameworks="$app/Contents/Frameworks"
is_external() { [[ "$1" == /opt/homebrew/* || "$1" == /usr/local/* ]]; }

# Copy each Homebrew library the binary needs, transitively, and point every
# reference at the bundled copy.
queue=("$app/Contents/MacOS/frameback")
while ((${#queue[@]})); do
    file=${queue[0]}
    queue=("${queue[@]:1}")
    for dep in $(otool -L "$file" | awk 'NR > 1 { print $1 }'); do
        is_external "$dep" || continue
        name=$(basename "$dep")
        if [[ ! -e "$frameworks/$name" ]]; then
            cp -L "$dep" "$frameworks/$name"
            chmod u+w "$frameworks/$name"
            install_name_tool -id "@rpath/$name" "$frameworks/$name" 2>/dev/null
            queue+=("$frameworks/$name")
        fi
        install_name_tool -change "$dep" "@rpath/$name" "$file" 2>/dev/null
    done
done
install_name_tool -add_rpath "@executable_path/../Frameworks" "$app/Contents/MacOS/frameback"
for lib in "$frameworks"/*.dylib; do
    install_name_tool -add_rpath "@loader_path" "$lib" 2>/dev/null || true
done

# Nothing may still point outside the app.
leftover=$(for f in "$app/Contents/MacOS/frameback" "$frameworks"/*.dylib; do
    otool -L "$f" | awk 'NR > 1 { print $1 }' | while read -r dep; do
        if is_external "$dep"; then echo "$f -> $dep"; fi
    done
done)
if [[ -n "$leftover" ]]; then
    echo "Libraries still outside the app:"; echo "$leftover"; exit 1
fi

identity=${FRAMEBACK_SIGN_IDENTITY:--}
codesign --force --sign "$identity" "$frameworks"/*.dylib
codesign --force --sign "$identity" "$app"
codesign --verify --deep --strict "$app"
echo "Built $app (v$version, $(du -sh "$app" | cut -f1), signed: $identity)"
