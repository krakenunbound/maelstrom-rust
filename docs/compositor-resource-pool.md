# Compositor resource pool

The project-monitor compositor retains steady-state GPU resources and now reuses complete resource
bundles when source or canvas dimensions recur. The public upload, generation, front/back output,
and pixel contracts are unchanged.

## Ownership and limits

- Active resources remain one straight plus one premultiplied texture for each of four fixed media
  layer slots and one double-buffered output pair.
- Retired layer bundles and output pairs are keyed by exact `PixelSize`. A source-size change,
  invisible/source-changed slot, canvas resize, or temporary absent frame can make a bundle
  reusable.
- The free pool has one shared 32 MiB binary logical-payload cap, at most four layer bundles, and
  at most one output pair. Oldest resources are evicted first. Entries larger than the cap are
  released rather than retained, so practical 4K bundles are deliberately not pooled.
- Full compositor clear releases active and pooled resources. It remains the hard lifecycle and
  memory-release boundary.
- Pool accounting includes texture pixels and known correction/curve buffer payloads. It is not a
  physical-VRAM measurement: drivers may add alignment, tiling, metadata, and object overhead.

The renderer also retains fixed vertex, matte, and layer-count CPU scratch plus its existing GPU
buffers. `egui_wgpu` supplies the render callback's command encoder; the compositor creates no
command buffers, so there is no compositor-owned command-buffer allocation to pool.

## Safety and verification

Resource mutation is confined to the renderer callback-resource thread. Queue submission order and
wgpu-owned resource references preserve earlier submitted work while a retired bundle is reused by
a later submission.

- Unit tests cover byte accounting, the shared byte cap, and entry limits.
- `gpu_compositor_reuses_exact_size_resources_and_rejects_oversize_pool_entries` queues layer and
  output size oscillation without waiting between submissions. It requires exact-size reuse after
  resize, layer clear, and an absent frame; proves oversize rejection, deterministic eviction and
  full-clear purge; and reads back the final frame after all queued reuse.
- Existing premultiplied-edge, matte-order, generation-skip, and full workspace release gates remain
  unchanged.

Memory-pressure measurement, physical-VRAM telemetry, practical 4K pooling, and cross-adapter
performance evidence remain open. The pool is a bounded churn optimization, not a claim that those
separate gates are complete.
