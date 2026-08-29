#!/usr/bin/env bash
set -euo pipefail

FFMPEG_COMMIT="9047fa1b084f76b1b4d065af2d743df1b40dfb56"
FFMPEG_VERSION="8.1"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
OUTPUT="$REPO_ROOT/.deps/ffmpeg-project-8.1-macos-arm64"
EXPECTED_OUTPUT="$REPO_ROOT/.deps/ffmpeg-project-8.1-macos-arm64"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "This recipe targets macOS 15+ on Apple Silicon." >&2
    exit 1
fi
if [[ "$OUTPUT" != "$EXPECTED_OUTPUT" ]]; then
    echo "Refusing destructive replacement outside the fixed macOS FFmpeg bundle." >&2
    exit 1
fi
for tool in git make xcrun shasum; do
    command -v "$tool" >/dev/null || { echo "Missing build tool: $tool" >&2; exit 1; }
done

BUILD_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/maelstrom-ffmpeg.XXXXXX")"
trap 'rm -rf -- "$BUILD_ROOT"' EXIT

git -C "$BUILD_ROOT" init --quiet ffmpeg
git -C "$BUILD_ROOT/ffmpeg" remote add origin https://github.com/FFmpeg/FFmpeg.git
for attempt in 1 2 3; do
    if git -C "$BUILD_ROOT/ffmpeg" fetch --quiet --depth 1 origin "$FFMPEG_COMMIT"; then
        break
    fi
    [[ "$attempt" -lt 3 ]] || exit 1
    sleep 2
done
git -C "$BUILD_ROOT/ffmpeg" checkout --quiet --detach FETCH_HEAD
[[ "$(git -C "$BUILD_ROOT/ffmpeg" rev-parse HEAD)" == "$FFMPEG_COMMIT" ]]
git -C "$BUILD_ROOT/ffmpeg" tag -f "n$FFMPEG_VERSION" "$FFMPEG_COMMIT" >/dev/null

rm -rf -- "$OUTPUT"
mkdir -p -- "$OUTPUT"
pushd "$BUILD_ROOT/ffmpeg" >/dev/null
./configure \
    --prefix="$OUTPUT" \
    --cc="$(xcrun --find clang)" \
    --arch=arm64 \
    --target-os=darwin \
    --enable-shared \
    --disable-static \
    --disable-debug \
    --disable-doc \
    --disable-autodetect \
    --enable-audiotoolbox \
    --enable-videotoolbox \
    --install-name-dir=@rpath \
    --extra-version=maelstrom-20260824 \
    --extra-cflags=-mmacosx-version-min=15.0 \
    --extra-ldflags=-mmacosx-version-min=15.0
make -j"$(sysctl -n hw.logicalcpu)"
make install
popd >/dev/null

cp "$BUILD_ROOT/ffmpeg/COPYING.LGPLv2.1" "$OUTPUT/LICENSE.txt"
CONFIGURATION="$($OUTPUT/bin/ffmpeg -hide_banner -version 2>&1)"
grep -q 'ffmpeg version n8.1-maelstrom-20260824' <<<"$CONFIGURATION"
grep -q -- '--enable-shared' <<<"$CONFIGURATION"
if grep -Eq -- '--enable-(gpl|nonfree)' <<<"$CONFIGURATION"; then
    echo "Refusing GPL/nonfree FFmpeg configuration." >&2
    exit 1
fi
ENCODERS="$($OUTPUT/bin/ffmpeg -hide_banner -encoders 2>/dev/null)"
for encoder in h264_videotoolbox aac; do
    grep -q " $encoder " <<<"$ENCODERS" || {
        echo "Required encoder missing: $encoder" >&2
        exit 1
    }
done
HWACCELS="$($OUTPUT/bin/ffmpeg -hide_banner -hwaccels 2>/dev/null)"
grep -q '^videotoolbox$' <<<"$HWACCELS" || {
    echo "Required hardware accelerator missing: videotoolbox" >&2
    exit 1
}

cat >"$OUTPUT/BUILD-MANIFEST.txt" <<EOF
Maelstrom project-built FFmpeg runtime
Platform: macOS 15+ arm64
FFmpeg version: $FFMPEG_VERSION
FFmpeg commit: $FFMPEG_COMMIT
License policy: LGPL shared libraries; GPL and nonfree disabled
Minimum macOS: 15.0
Hardware media: VideoToolbox and AudioToolbox

$CONFIGURATION
EOF

(
    cd "$OUTPUT"
    find bin lib \( -type f -o -type l \) -print0 \
        | LC_ALL=C sort -z \
        | xargs -0 shasum -a 256 \
        > BUILD-SHA256SUMS.txt
)
echo "Built $OUTPUT"
