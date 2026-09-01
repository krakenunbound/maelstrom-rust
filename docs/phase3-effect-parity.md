# Phase 3 native-preview/export effect parity

## Contract

The Phase 3 exit gate compares the same evaluated effect stack at the encoded-RGBA effect
boundary. The native side uses the real `nle-render` GPU compositor. The export side uses the
production `ExportPlan`, compiled graph order, and FFmpeg effect lowering, but replaces the final
H.264/YUV delivery boundary with one lossless RGBA frame. This separates effect-graph correctness
from later codec, output-color, and compositor-delivery qualifications.

The ignored app qualification creates a lossless 64x48, 24 fps FFV1/BGRA source with deterministic
full-range red, green, and blue gradients, then evaluates tick 500,000. Its two-node graph contains:

- animated Brightness/Contrast;
- non-identity master, red, green, and blue natural curves;
- animated Vignette amount, midpoint, feather, and center.

The neutral production export boundary is rendered first and supplied to the GPU test, so both
effect evaluators receive identical encoded bytes. The neutral source/export boundary is checked
independently against the same four-code-value tolerance. Every pixel and alpha channel in the
effect outputs is then compared; the allowed maximum absolute 8-bit error is 4.

## Local evidence

On 2026-08-31, the explicit release qualification ran on the NVIDIA GeForce RTX 3090 Vulkan
adapter with the approved project-local FFmpeg 8.1 runtime:

```text
graph_nodes=2 size=64x48 tick=500000
effect max_error=0 tolerance=4
neutral boundary max_error=1 tolerance=4
```

The gate is:

```text
cargo test -p nle-app --release phase3_native_preview_matches_export_graph_pixels_for_animated_color_stack -- --ignored --nocapture
```

It requires the approved `FFMPEG_DIR` and `LIBCLANG_PATH` environment described by the workspace
rules. The editor GUI is not launched.

## Corrections established by the gate

The first end-to-end runs exposed real boundary differences rather than just test noise:

- native curve tables retained floating spline values while FFmpeg uses byte component tables and
  applies the master table after component quantization;
- native nodes rounded or retained fractional values where FFmpeg `geq` truncates at an 8-bit node
  boundary;
- zero-degree export rotation performed an unnecessary premultiply/rotate/unpremultiply round trip;
- FFmpeg `overlay` defaulted to YUV 4:2:0, silently chroma-subsampling RGBA video, matte, and title
  composition before final delivery.

The native renderer now follows FFmpeg's byte-LUT/node boundaries. Export skips the zero-degree
rotation round trip and explicitly composes video, dip mattes, and titles in RGB. Normal export still
ends at its authored H.264 `yuv420p` delivery boundary.

## Scope boundary

This closes the Phase 3 effect-graph exit gate. It does not claim H.264 codec parity, a linear-light
working space, HDR/Rec.709 output conformance, arbitrary transformed multilayer parity, or complete
transition/title/color-pipeline parity. Those remain Phase 4 and export-quality work.
