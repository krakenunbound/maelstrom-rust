# Preview sampling

Maelstrom exposes preview sampling as a separate setting from playback resolution:

- **Nearest** preserves hard pixel boundaries and uses FFmpeg point sampling.
- **Bilinear** uses two-dimensional linear interpolation.
- **Bicubic** is the default and uses FFmpeg bicubic scaling plus an explicit Catmull-Rom filter
  in the retained native viewer compositor.

The setting is under **Playback > Sampling Quality** and is available in English and Japanese.
It does not alter moving or paused playback resolution, the adaptive `Auto` resolution policy, or
export. Export continues to use the render graph's separately defined sampling contract.

## Persistence and compatibility

New projects and snapshots default to Bicubic. Snapshots save the three-way value explicitly.
Projects written before the setting existed migrate the former `High Quality Playback` boolean:

- enabled becomes Bicubic;
- disabled becomes Bilinear.

The legacy boolean remains a serialized compatibility mirror so older readers receive a coherent
value. Runtime compatibility methods map only Bicubic to enabled.

## Decode and cache identity

The selected method is part of every monitor request, decoded-frame cache key, sticky scaler
session, and hardware-transfer scaler identity. Switching methods cancels obsolete work and cannot
reuse pixels produced by another filter. The pinned FFmpeg scaler flags are:

- Nearest: `POINT | ACCURATE_RND`
- Bilinear: `BILINEAR | ACCURATE_RND`
- Bicubic: `BICUBIC | ACCURATE_RND`

Changing sampling does not change the requested raster dimensions and never silently lowers
playback resolution.

## Native viewer compositor

Nearest and Bilinear use retained clamp-to-edge WebGPU samplers. WebGPU has no native bicubic
sampler, so Bicubic uses a 16-tap Catmull-Rom WGSL filter over the already encoded-premultiplied
layer texture. Filtered alpha is clamped to `[0, 1]` and RGB to `[0, alpha]`, preserving the
premultiplied-alpha invariant at transparent edges. The upload premultiply pass uses exact
`textureLoad` texels, so the presentation setting cannot modify source pixels during upload.

All samplers, layouts, bind groups, shaders, and pipelines are retained. A setting change advances
only the compositor input generation, immediately recomposing an already uploaded frame without a
new decode, upload, shader compilation, or pipeline compilation.

The emergency egui texture fallback exposes only nearest and linear presentation filters. It maps
Nearest to nearest and Bilinear/Bicubic to linear; the Bicubic decoder raster is still preserved.
This limitation is explicit and is not presented as native bicubic support.

## Verification and remaining qualification

Focused tests cover bilingual state and migration, all FFmpeg mappings, cache/scaler invalidation,
the three app mappings, WGSL parsing, retained-frame recomposition, and premultiplied bicubic edge
readback on a real local GPU. Release workspace tests and strict Clippy remain the integration gate.

This work does not prove physical scanout, practical four-layer 4K bicubic performance, or broad
Intel/NVIDIA/AMD cross-hardware performance. Those remain part of the roadmap's Phase 0/Phase 2
hardware qualification rather than grounds for silently changing the user's selected sampling or
resolution.
