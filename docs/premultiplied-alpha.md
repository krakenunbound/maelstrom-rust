# Premultiplied-alpha contract

Maelstrom keeps decoded and generated CPU images as straight (unassociated) RGBA8. The viewer and
export paths temporarily premultiply only where filtered sampling or compositing requires it. This
prevents transparent-edge RGB from becoming a dark fringe while preserving the existing asset and
FFmpeg overlay boundaries.

## Viewer

- A changed straight-RGBA upload is written once to a retained source texture.
- A retained GPU pass creates the encoded-sRGB premultiplied texture used by scale, rotation,
  opacity, color correction, and source-over composition. Unchanged frames do not repeat this pass.
- Media layers and project mattes use explicit premultiplied source-over blending.
- The compositor target retains encoded-sRGB values. Presentation chooses the shader entry point
  from the actual surface format: sRGB targets decode before the target re-encodes, while non-sRGB
  fallback targets write the encoded value directly.

This Phase 2 contract is deliberately encoded-sRGB. It does not claim the linear-light Rec.709
working space planned for Phase 4.

## Export

Generated FFmpeg graphs convert straight RGBA to premultiplied RGBA before filtered scale or
rotation and return to straight RGBA afterward. FFmpeg `overlay` boundaries explicitly use
`alpha=straight`, including media, title, and dip-matte overlays. Pointwise encoded color and
opacity operations remain between the resampling boundaries so preview and export share the same
current-space semantics.

The pinned runtime double-attenuates a partially transparent cross-dissolve when fed premultiplied
content through `overlay=alpha=premultiplied`; therefore Maelstrom intentionally restores straight
RGBA before its overlay boundary. The generated cross-dissolve regression checks balanced red and
blue energy at the midpoint.

## Verification

- Renderer unit tests validate both shader contracts and surface-format dispatch.
- `gpu_filters_premultiplied_edges_without_dark_halos` performs real GPU readbacks of a filtered
  opaque-red/transparent-black edge and of both sRGB and non-sRGB presentation targets.
- Export regressions run the pinned FFmpeg runtime against transparent filtered edges, generated
  still-image graphs, cross dissolves, transformed layers, titles, and dip mattes.
- The full release workspace suite, strict Clippy, and formatting remain required before commit.

## Remaining limits

The retained straight and premultiplied textures add one RGBA texture per active media layer. Four
3840x2160 layers add about 132 MiB before driver allocation overhead. Texture pooling,
memory-pressure qualification, cross-adapter performance evidence, device-loss stress, and a
numerical end-to-end preview/export parity fixture remain open gates. Device recreation rebuilds
the renderer and marks new uploads dirty; there is no separate retry path for a command buffer
dropped before submission.
