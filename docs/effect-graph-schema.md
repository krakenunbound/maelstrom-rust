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

Timeline restoration rejects unsupported graph versions, excessive or duplicate nodes, zero IDs,
invalid effect parameters, missing edge nodes or ports, wrong port directions, incompatible value
types, duplicate edges, cycles, branching, and noncanonical order. Unknown graph fields are rejected
instead of being silently discarded. Structural edits rebuild canonical connections immediately,
and serialization validates a copy before writing.

This schema change does not alter rendering. Preview and export still evaluate the same bounded
ordered nodes through their existing paths. A compiled runtime graph, worker-side compilation,
derived caches, arbitrary DAG execution, and a larger effects catalog remain separate roadmap work.

