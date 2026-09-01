# Effect graph schema v1

Maelstrom stores each clip's video effects as a versioned graph owned by `nle-timeline`. Project
document version 8 is the first writer of this representation. Versions 1–7 remain readable:
their ordered `video_effects` arrays are converted to canonical schema-v1 chains while loading.

## Durable contract

Each graph contains:

- `schema_version`, currently `1`;
- ordered nodes with stable clip-local `VideoEffectId` values and typed effect data;
- explicit connections between stable port IDs.

Effect kinds expose stable parameter IDs and value types. Parameter values remain in their
effect-specific Rust structures, so graph metadata cannot disagree with the values used by preview
or export. Existing numeric IDs must not be repurposed.

Schema v1 deliberately permits only one canonical video-frame chain. Node order is execution order,
and every adjacent pair must have one `VIDEO_OUTPUT` to `VIDEO_INPUT` connection. Empty and
single-node graphs are valid. Branching and arbitrary parameter links are reserved for a later
schema.

## Validation and compatibility

Timeline restoration accepts at most ten ordered nodes per clip and rejects unsupported graph
versions, excessive or duplicate nodes, zero IDs,
invalid effect parameters, missing edge nodes or ports, wrong port directions, incompatible value
types, duplicate edges, cycles, branching, and noncanonical order. Unknown graph fields are rejected
instead of being silently discarded. Structural edits rebuild canonical connections immediately,
and serialization validates a copy before writing.

## Compiled runtime contract

The durable graph remains project truth. `VideoEffectGraph::compile` clones, normalizes, and
validates that truth into a separate immutable operation sequence. The compiled plan retains stable
node identity and full animated parameters for export, while precomputing the fixed-size RGB curve
representation used by repeated preview evaluation. Compiled plans and derived data are never
serialized.

The app compiles only the active bounded viewer set on a dedicated latest-wins worker. Requests are
tagged with the timeline generation; pending and completed queues are capped at the four-layer
viewer limit. The owner thread installs an `Arc` only when both the generation and source graph
still match, replacing the complete entry in one assignment. Until then, preview uses the existing
direct evaluator, so an edit cannot display stale effects. The runtime cache is recency-bounded to
four clips and is empty after project restore. Worker polling and compilation remain outside the
render hot path.

Export compiles each video clip once while building its immutable export plan, then lowers that
same canonical compiled operation order to source-time FFmpeg expressions. Invalid graphs fail
export preflight normally instead of being skipped or causing a panic. Existing preview and export
results remain unchanged.

## Preview/export parity gate

The Phase 3 qualification evaluates an animated Brightness/Contrast + RGB-curves + Vignette stack
through both the real native GPU compositor and the production FFmpeg graph lowering. Both sides
receive the same neutral encoded-RGBA effect input. A full-frame comparison permits at most four
8-bit code values of color/rounding error; the local RTX 3090/Vulkan qualification measured zero.
The neutral source/export boundary independently measured one. See
`docs/phase3-effect-parity.md` for the fixture, command, corrections, and scope limits.

Arbitrary DAG execution, parameter links, output-size/color-setting cache keys, renderer resource
caches, and a larger effects catalog remain separate roadmap work.
