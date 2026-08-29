# Model registry and licensing record

The public Maelstrom repository contains no model weights. `manifest.json` is the tracked registry
contract and remains empty until a model has both a concrete code consumer and documented
redistribution permission. Model weights and common inference-package extensions are ignored by Git.

## How the code uses this directory

`crates/nle-app/src/model_preload.rs` selects the directory named by `MAELSTROM_MODEL_DIR`, or the
`models` directory beside the running executable when no override is set. During startup it parses
`manifest.json`, validates each entry, reads valid files into reference-counted memory, and reports
invalid entries without discarding valid peers. The Windows packaging script validates the same
registry and copies it beside `Maelstrom.exe`.

No inference engine currently consumes a model ID. The checked-in empty registry is therefore the
only accurate default; preloading is infrastructure for a future, explicitly licensed integration.

## Manifest version 1

```json
{
  "version": 1,
  "models": [
    {
      "id": "stable-code-facing-id",
      "file": "vendor/model-file.onnx",
      "expected_bytes": 123456
    }
  ]
}
```

- `id` is a non-empty stable identifier and must be unique.
- `file` is a safe relative path below this directory; absolute paths and `..` are rejected.
- `expected_bytes` is optional but strongly recommended as a corruption check.
- The manifest is limited to 1 MiB and 64 entries.

## Required provenance record

Before adding a non-empty entry, add a nearby text record containing:

- model name, version, and immutable download or release URL;
- author/vendor and original project page;
- license name, license URL, and a local copy when its terms require one;
- whether modification, commercial use, and redistribution are permitted;
- SHA-256 and exact byte length of the local artifact;
- the Maelstrom crate/module and model ID that consume it;
- acquisition date and any conversion or quantization steps.

Keep unredistributable weights in an external directory and point `MAELSTROM_MODEL_DIR` to it. Never
commit a weight merely because it is downloadable. See `docs/OPTIONAL_RUNTIME_ASSETS.md` for the
separate NVIDIA RTX VSR native-runtime dependency, which is not part of this model registry.
