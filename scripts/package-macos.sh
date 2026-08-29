#!/usr/bin/env bash
set -euo pipefail

FFMPEG_COMMIT="9047fa1b084f76b1b4d065af2d743df1b40dfb56"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
FFMPEG_ROOT="${1:-$REPO_ROOT/.deps/ffmpeg-project-8.1-macos-arm64}"
FFMPEG_ROOT="$(cd -- "$FFMPEG_ROOT" && pwd -P)"
DIST_ROOT="$REPO_ROOT/dist"
PACKAGE_ROOT="$DIST_ROOT/Maelstrom-macOS-arm64"
APP="$PACKAGE_ROOT/Maelstrom.app"
MACOS="$APP/Contents/MacOS"
FRAMEWORKS="$APP/Contents/Frameworks"
RESOURCES="$APP/Contents/Resources"
SIGN_IDENTITY="${CODESIGN_IDENTITY:--}"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "This package recipe targets macOS 15+ on Apple Silicon." >&2
    exit 1
fi
case "$PACKAGE_ROOT" in
    "$DIST_ROOT"/*) ;;
    *) echo "Refusing package output outside $DIST_ROOT" >&2; exit 1 ;;
esac
for tool in cargo codesign install_name_tool otool pgrep plutil realpath shasum sips iconutil; do
    command -v "$tool" >/dev/null || { echo "Missing package tool: $tool" >&2; exit 1; }
done

MANIFEST="$FFMPEG_ROOT/BUILD-MANIFEST.txt"
CHECKSUMS="$FFMPEG_ROOT/BUILD-SHA256SUMS.txt"
[[ -x "$FFMPEG_ROOT/bin/ffmpeg" && -x "$FFMPEG_ROOT/bin/ffprobe" ]]
[[ -f "$MANIFEST" && -f "$CHECKSUMS" && -f "$FFMPEG_ROOT/LICENSE.txt" ]]
grep -q "FFmpeg commit: $FFMPEG_COMMIT" "$MANIFEST"
grep -q 'Platform: macOS 15+ arm64' "$MANIFEST"

while IFS= read -r line; do
    [[ "$line" =~ ^([0-9a-f]{64})[[:space:]][[:space:]](.+)$ ]] || {
        echo "Malformed FFmpeg checksum entry: $line" >&2; exit 1;
    }
    expected="${BASH_REMATCH[1]}"
    relative="${BASH_REMATCH[2]}"
    case "$relative" in
        bin/*|lib/*) ;;
        *) echo "Unsafe FFmpeg checksum target: $relative" >&2; exit 1 ;;
    esac
    artifact="$FFMPEG_ROOT/$relative"
    [[ -f "$artifact" ]] || { echo "Missing FFmpeg artifact: $relative" >&2; exit 1; }
    resolved="$(realpath "$artifact")"
    case "$resolved" in
        "$FFMPEG_ROOT"/*) ;;
        *) echo "FFmpeg artifact escapes its bundle: $relative" >&2; exit 1 ;;
    esac
    actual="$(shasum -a 256 "$artifact" | awk '{print $1}')"
    [[ "$actual" == "$expected" ]] || { echo "FFmpeg checksum mismatch: $relative" >&2; exit 1; }
done < "$CHECKSUMS"

CONFIGURATION="$($FFMPEG_ROOT/bin/ffmpeg -hide_banner -version 2>&1)"
grep -q 'ffmpeg version n8.1-maelstrom-20260824' <<<"$CONFIGURATION"
grep -q -- '--enable-shared' <<<"$CONFIGURATION"
if grep -Eq -- '--enable-(gpl|nonfree)' <<<"$CONFIGURATION"; then
    echo "Refusing GPL/nonfree FFmpeg configuration." >&2
    exit 1
fi

export FFMPEG_DIR="$FFMPEG_ROOT"
export PKG_CONFIG_PATH="$FFMPEG_ROOT/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export DYLD_FALLBACK_LIBRARY_PATH="$FFMPEG_ROOT/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
export LIBCLANG_PATH="${LIBCLANG_PATH:-$(xcode-select -p)/Toolchains/XcodeDefault.xctoolchain/usr/lib}"
(cd "$REPO_ROOT" && cargo build -p nle-app --release)

rm -rf -- "$PACKAGE_ROOT"
mkdir -p -- "$MACOS" "$FRAMEWORKS" "$RESOURCES"
cp "$REPO_ROOT/target/release/nle-app" "$MACOS/Maelstrom"
cp "$FFMPEG_ROOT/bin/ffmpeg" "$MACOS/ffmpeg"
cp "$FFMPEG_ROOT/bin/ffprobe" "$MACOS/ffprobe"
cp -a "$FFMPEG_ROOT/lib/"*.dylib* "$FRAMEWORKS/"
cp "$REPO_ROOT/THIRD_PARTY_NOTICES.md" "$RESOURCES/"
cp "$FFMPEG_ROOT/LICENSE.txt" "$RESOURCES/FFmpeg-LICENSE.txt"
cp "$MANIFEST" "$CHECKSUMS" "$RESOURCES/"

cat >"$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>Maelstrom</string>
  <key>CFBundleExecutable</key><string>Maelstrom</string>
  <key>CFBundleIconFile</key><string>Maelstrom.icns</string>
  <key>CFBundleIdentifier</key><string>com.krakenunbound.maelstrom</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Maelstrom</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
plutil -lint "$APP/Contents/Info.plist"

ICONSET="$(mktemp -d "${TMPDIR:-/tmp}/maelstrom-icon.XXXXXX")/Maelstrom.iconset"
mkdir -p "$ICONSET"
ICON_SOURCE="$REPO_ROOT/assets/branding/maelstrom-app-icon.png"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$ICON_SOURCE" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$ICON_SOURCE" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$RESOURCES/Maelstrom.icns"
rm -rf -- "$(dirname "$ICONSET")"

add_rpath() {
    local binary="$1" load_commands
    load_commands="$(otool -l "$binary")"
    if [[ "$load_commands" != *"@executable_path/../Frameworks"* ]]; then
        install_name_tool -add_rpath '@executable_path/../Frameworks' "$binary"
    fi
}
rewrite_dependencies() {
    local binary="$1" dependency base
    while IFS= read -r dependency; do
        case "$dependency" in
            "$FFMPEG_ROOT"/lib/*)
                base="$(basename "$dependency")"
                install_name_tool -change "$dependency" "@rpath/$base" "$binary"
                ;;
            @rpath/*|/System/Library/*|/usr/lib/*) ;;
            *) echo "Unbundled dynamic dependency in $binary: $dependency" >&2; exit 1 ;;
        esac
    done < <(otool -L "$binary" | tail -n +2 | awk '{print $1}')
}
rewrite_identity() {
    local dylib="$1" identity base
    identity="$(otool -D "$dylib" | tail -n 1)"
    case "$identity" in
        "$FFMPEG_ROOT"/lib/*)
            base="$(basename "$identity")"
            install_name_tool -id "@rpath/$base" "$dylib"
            ;;
        @rpath/*) ;;
        *) echo "Unexpected dynamic-library identity in $dylib: $identity" >&2; exit 1 ;;
    esac
}

while IFS= read -r -d '' dylib; do
    rewrite_identity "$dylib"
    rewrite_dependencies "$dylib"
done < <(find "$FRAMEWORKS" -type f -name '*.dylib' -print0)
for binary in "$MACOS/Maelstrom" "$MACOS/ffmpeg" "$MACOS/ffprobe"; do
    rewrite_dependencies "$binary"
    add_rpath "$binary"
done

sign_args=(--force --sign "$SIGN_IDENTITY")
if [[ "$SIGN_IDENTITY" != "-" ]]; then
    sign_args+=(--options runtime --timestamp)
fi
while IFS= read -r -d '' dylib; do
    codesign "${sign_args[@]}" "$dylib"
done < <(find "$FRAMEWORKS" -type f -name '*.dylib' -print0)
codesign "${sign_args[@]}" "$MACOS/ffmpeg"
codesign "${sign_args[@]}" "$MACOS/ffprobe"
codesign "${sign_args[@]}" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin DYLD_FALLBACK_LIBRARY_PATH= \
    "$MACOS/ffmpeg" -hide_banner -version >/dev/null
env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin DYLD_FALLBACK_LIBRARY_PATH= \
    "$MACOS/ffprobe" -hide_banner -version >/dev/null

ANALYSIS_SMOKE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/maelstrom-analysis.XXXXXX")"
cleanup_analysis_smoke() {
    rm -rf -- "$ANALYSIS_SMOKE_DIR"
}
trap cleanup_analysis_smoke EXIT
ANALYSIS_MEDIA="$ANALYSIS_SMOKE_DIR/analysis-smoke.mp4"
ANALYSIS_PROBE="$ANALYSIS_SMOKE_DIR/probe.json"
env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin DYLD_FALLBACK_LIBRARY_PATH= \
    "$MACOS/ffmpeg" -hide_banner -loglevel error -y \
    -f lavfi -i testsrc2=size=320x180:rate=24 \
    -f lavfi -i sine=frequency=440:sample_rate=48000 \
    -t 60 -c:v h264_videotoolbox -allow_sw 1 -pix_fmt yuv420p -c:a aac \
    "$ANALYSIS_MEDIA"
env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin DYLD_FALLBACK_LIBRARY_PATH= \
    "$MACOS/ffprobe" -v error -show_streams -show_format -of json \
    "$ANALYSIS_MEDIA" >"$ANALYSIS_PROBE"
env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin DYLD_FALLBACK_LIBRARY_PATH= \
    "$MACOS/ffmpeg" -hide_banner -loglevel error -nostdin \
    -hwaccel videotoolbox -i "$ANALYSIS_MEDIA" \
    -map 0:v:0 -frames:v 3 -f null -
env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin DYLD_FALLBACK_LIBRARY_PATH= \
    "$MACOS/ffmpeg" -hide_banner -loglevel error -nostdin -i "$ANALYSIS_MEDIA" \
    -map 0:a:0 -t 0.25 -f f32le /dev/null
python3 - "$ANALYSIS_PROBE" <<'PY'
import json, sys
probe = json.load(open(sys.argv[1], encoding="utf-8"))
types = {stream.get("codec_type") for stream in probe.get("streams", [])}
assert {"audio", "video"} <= types, probe
assert float(probe["format"]["duration"]) >= 59.0, probe
PY

STARTUP_REPORT="$DIST_ROOT/last-startup-smoke-macos.json"
SURFACE_REPORT="$DIST_ROOT/last-surface-submission-smoke-macos.json"
ACCEPTANCE_REPORT="$DIST_ROOT/last-media-acceptance-smoke-macos.json"
ACCEPTANCE_EXPORT="$ANALYSIS_SMOKE_DIR/acceptance-export.mp4"
rm -f -- "$STARTUP_REPORT" "$SURFACE_REPORT" "$ACCEPTANCE_REPORT" "$ACCEPTANCE_EXPORT"
env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME="$HOME" \
    MAELSTROM_SMOKE_EDITOR=1 \
    MAELSTROM_STARTUP_REPORT="$STARTUP_REPORT" \
    MAELSTROM_SURFACE_SUBMISSION_REPORT="$SURFACE_REPORT" \
    MAELSTROM_MEDIA_ACCEPTANCE_PATH="$ANALYSIS_MEDIA" \
    MAELSTROM_MEDIA_ACCEPTANCE_REPORT="$ACCEPTANCE_REPORT" \
    MAELSTROM_MEDIA_ACCEPTANCE_EXPORT_PATH="$ACCEPTANCE_EXPORT" \
    "$MACOS/Maelstrom" >"$DIST_ROOT/macos-smoke.log" 2>&1 &
smoke_pid=$!
terminate_process_tree() {
    local parent="$1" child
    while IFS= read -r child; do
        [[ -n "$child" ]] && terminate_process_tree "$child"
    done < <(pgrep -P "$parent" 2>/dev/null || true)
    kill "$parent" 2>/dev/null || true
}
cleanup_smoke() {
    if kill -0 "$smoke_pid" 2>/dev/null; then terminate_process_tree "$smoke_pid"; fi
    wait "$smoke_pid" 2>/dev/null || true
}
trap 'cleanup_smoke; cleanup_analysis_smoke' EXIT
deadline=$((SECONDS + 60))
while [[ (! -f "$STARTUP_REPORT" || ! -f "$SURFACE_REPORT" || ! -f "$ACCEPTANCE_REPORT") && $SECONDS -lt $deadline ]]; do
    kill -0 "$smoke_pid" 2>/dev/null || { cat "$DIST_ROOT/macos-smoke.log" >&2; exit 1; }
    sleep 0.1
done
[[ -f "$STARTUP_REPORT" ]] || { cat "$DIST_ROOT/macos-smoke.log" >&2; echo "Startup presentation smoke timed out." >&2; exit 1; }
[[ -f "$SURFACE_REPORT" ]] || { cat "$DIST_ROOT/macos-smoke.log" >&2; echo "Surface submission smoke timed out." >&2; exit 1; }
[[ -f "$ACCEPTANCE_REPORT" ]] || { cat "$DIST_ROOT/macos-smoke.log" >&2; echo "Real-media acceptance smoke timed out." >&2; exit 1; }
python3 - "$STARTUP_REPORT" "$SURFACE_REPORT" "$ACCEPTANCE_REPORT" <<'PY'
import json, sys
startup = json.load(open(sys.argv[1], encoding="utf-8"))
surface = json.load(open(sys.argv[2], encoding="utf-8"))
acceptance = json.load(open(sys.argv[3], encoding="utf-8"))
assert 0 <= startup["first_surface_present_ms"] < 1000.0, startup
assert surface["samples"] >= 120, surface
assert 0 <= surface["cpu_p95_ms"] <= 8.0, surface
assert surface["average_submission_fps"] >= 55.0, surface
assert 0 <= surface["surface_submission_interval_p95_ms"] <= 25.0, surface
for key in (
    "schema_version", "renderer_gpu_name", "renderer_vendor_id", "renderer_device_id",
    "renderer_backend", "renderer_driver", "renderer_driver_info", "decoder_backends",
    "encoder_backend", "cpu_identity", "logical_cpu_count", "total_physical_memory_bytes",
    "selected_preview_quality", "resolved_preview_quality", "preview_width", "preview_height",
    "monitor_cache_cap_bytes", "display_refresh_millihertz",
):
    assert key in surface, (key, surface)
assert surface["schema_version"] == 1, surface
assert surface["renderer_gpu_name"] and surface["renderer_backend"], surface
assert surface["decoder_backends"] and surface["encoder_backend"] != "not_observed", surface
assert surface["logical_cpu_count"] >= 1, surface
assert surface["cpu_identity"] is None or surface["cpu_identity"], surface
assert surface["total_physical_memory_bytes"] is None or surface["total_physical_memory_bytes"] >= 1, surface
assert surface["preview_width"] >= 1 and surface["preview_height"] >= 1, surface
assert surface["monitor_cache_cap_bytes"] >= 1, surface
assert surface["display_refresh_millihertz"] is None or surface["display_refresh_millihertz"] >= 1, surface
for key in (
    "media_pool_drag_completed", "analysis_metadata_ready", "waveform_ready", "monitor_frame_arrived",
    "live_audio_meter_nonzero", "live_fade_reduced", "live_fade_recovered", "live_gain_reduced",
    "export_started", "export_progress_received",
    "playhead_advanced_while_exporting", "export_cancelled",
):
    assert acceptance.get(key) is True, (key, acceptance)
for key in ("viewer_panel_height", "timeline_panel_height", "timeline_view_span_ticks", "timeline_end_ticks", "linked_video_bars", "linked_audio_bars", "waveform_peak_count", "playhead_advanced_ticks"):
    assert acceptance.get(key, 0) > 0, (key, acceptance)
assert acceptance["timeline_panel_height"] > acceptance["viewer_panel_height"], acceptance
assert 59000000 <= acceptance["timeline_end_ticks"] <= 61000000, acceptance
assert acceptance["timeline_end_ticks"] <= acceptance["timeline_view_span_ticks"] <= 2 * acceptance["timeline_end_ticks"], acceptance
assert acceptance["playhead_advanced_ticks"] >= 500000, acceptance
print(f"Startup smoke: first successful surface presentation in {startup['first_surface_present_ms']} ms")
print(f"Surface submission smoke: {surface['average_submission_fps']} submissions/s, interval p95 {surface['surface_submission_interval_p95_ms']} ms, CPU p95 {surface['cpu_p95_ms']} ms (not scanout/GPU completion)")
print(f"Media acceptance smoke: timeline {acceptance['timeline_panel_height']} px > viewer {acceptance['viewer_panel_height']} px; fitted view {acceptance['timeline_view_span_ticks']}/{acceptance['timeline_end_ticks']} ticks; {acceptance['linked_video_bars']} V bars, {acceptance['linked_audio_bars']} A bars, {acceptance['waveform_peak_count']} waveform peaks, {acceptance['playhead_advanced_ticks']} ticks, export cancelled cleanly")
PY
[[ ! -e "$ACCEPTANCE_EXPORT" ]] || { echo "Cancelled packaged export left a partial output behind." >&2; exit 1; }
cleanup_smoke
cleanup_analysis_smoke
trap - EXIT

echo "Packaged $APP"
