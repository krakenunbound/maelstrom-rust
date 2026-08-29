# Optional proprietary runtime assets

Maelstrom's public source intentionally excludes third-party binaries and model weights that do not
have confirmed public-redistribution permission. This document records their source families, their
code integration, and the local restoration contract without redistributing them.

## NVIDIA RTX Video Super Resolution runtime

The optional RTX VSR integration is implemented in `crates/nle-upscale/src/rtx_vsr.rs`. It uses
`libloading` to load every required DLL at runtime, finds the NVIDIA Video Effects and CV Image C
exports, creates a persistent `VideoSuperRes` effect, transfers RGB frames through NVIDIA image
buffers, and returns the enhanced frames to `crates/nle-upscale/src/job.rs`. The app checks
`nle_upscale::capability` before exposing the upscale job.

Maelstrom does not contain or launch a separate upscaler executable.

### Runtime families and official sources

| Local files | Source family | Use in Maelstrom |
| --- | --- | --- |
| `NVVideoEffects.dll`, `NVCVImage.dll`, `nvVFXVideoSuperRes.dll` | [NVIDIA Maxine / Video Effects SDK](https://developer.nvidia.com/maxine/) | Provides `NvVFX_*`, `NvCVImage_*`, and the `VideoSuperRes` effect. |
| `nppc64_12.dll`, `nppi*64_12.dll` | [CUDA Toolkit NPP](https://docs.nvidia.com/cuda/npp/introduction.html) | NVIDIA image-processing dependencies loaded by the VSR runtime. |
| `nvinfer_10.dll`, `nvinfer_plugin_10.dll`, `nvonnxparser_10.dll` | [NVIDIA TensorRT](https://docs.nvidia.com/deeplearning/tensorrt/latest/installing-tensorrt/installing.html) | TensorRT inference and ONNX parser dependencies used by the vendor runtime. |
| `nvngxruntime.dll`, `nvngx_vsr.dll` | NVIDIA NGX / RTX VSR runtime | Vendor RTX VSR components. Reacquire only through an official NVIDIA SDK, application package, or driver distribution whose terms permit the intended local use. |

The exact acquisition package for the current local NGX files was not recorded when they were first
added. That provenance gap is documented rather than guessed. They must not be redistributed until
their exact source and redistribution terms are established.

The current local snapshot reports `nvngx_vsr.dll` 1.8.2.0 and `NVVideoEffects.dll` 1.2.0.0 in
Windows version metadata; the NPP files identify themselves as NVIDIA CUDA NPP libraries. Files
without reliable embedded versions are not assigned guessed versions. Exact integrity values are
tracked in [RTX_VSR_LOCAL_BUNDLE.sha256](./RTX_VSR_LOCAL_BUNDLE.sha256); the hashes identify the
known-working local bundle but do not grant redistribution rights.

### Required files

The loader currently requires all 17 names below before RTX VSR is considered available:

```text
NVCVImage.dll
NVVideoEffects.dll
nppc64_12.dll
nppial64_12.dll
nppicc64_12.dll
nppidei64_12.dll
nppif64_12.dll
nppig64_12.dll
nppim64_12.dll
nppist64_12.dll
nppitc64_12.dll
nvVFXVideoSuperRes.dll
nvinfer_10.dll
nvinfer_plugin_10.dll
nvngx_vsr.dll
nvngxruntime.dll
nvonnxparser_10.dll
```

### Runtime lookup order

`rtx_vsr::runtime_dir()` accepts the first directory containing all required files:

1. `KRAKEN_RTX_VSR_DIR`;
2. `rtx-vsr` beside the running executable;
3. `crates/nle-upscale/rtx-vsr` for a source-tree build;
4. `rtx-vsr` at the Cargo workspace root.

For local development, keep one restricted bundle outside Git and either set the environment
variable or create a local directory junction at `crates/nle-upscale/rtx-vsr`. That path and every
DLL are ignored. For a privately assembled package, place the same directory beside
`Maelstrom.exe`. The public packaging script does not copy these proprietary binaries.

The workspace's local `Launch-Maelstrom-Editor.bat` sets `KRAKEN_RTX_VSR_DIR` to the machine's
restricted store before starting the package by its full absolute path. It warns, but still allows
normal editing, when the optional runtime is absent.

### Local verification

From the repository root, this confirms that Rust can see a complete runtime without loading the
GPU libraries or starting Maelstrom:

```powershell
cargo test -p nle-upscale local_runtime_is_complete_when_present
```

The test is intentionally a no-op on public/CI machines where the optional directory is absent.

The current machine's restricted store also contains `README.md` and `SHA256SUMS.txt`. Those local
records identify the exact files in use and allow integrity verification without putting the files
in public Git history.

## AI/model weights

Model weights use the separate versioned registry described in `assets/models/README.md`. The public
manifest is empty and no inference engine currently consumes a model ID. Additions require an exact
source, license, checksum, size, code consumer, and confirmed redistribution status.
