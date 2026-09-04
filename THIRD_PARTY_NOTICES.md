# Third-party notices

Maelstrom dynamically links to project-built FFmpeg 8.1 shared libraries and
distributes the matching `ffmpeg` and `ffprobe` tools. Windows and macOS builds use
FFmpeg commit `9047fa1b084f76b1b4d065af2d743df1b40dfb56` (tag `n8.1`).
The Windows build additionally uses nv-codec-headers commit
`1889e62e2d35ff7aa9baca2bceb14f053785e6f1`
(tag `n12.1.14.0`) and the Intel oneVPL dispatcher from commit
`2274efcd3672b43297ef774f332e1fed6781381c` (tag `v2023.4.0`), plus a decoder-only,
statically linked libaom build from commit
`d9c115ce0951324dee243041ef810e27202de20f` (tag `v3.13.0`). FFmpeg is licensed under
the GNU Lesser General Public License, version 2.1 or later. Maelstrom does not enable
GPL or nonfree FFmpeg components and does not package x264, x265, FDK AAC, or a libaom
DLL. oneVPL is distributed under the MIT License. The bundled `libaom-LICENSE.txt` and
`libaom-PATENTS.txt` preserve libaom's upstream license and patent notice.

- FFmpeg project and source: https://ffmpeg.org/
- Exact FFmpeg source: https://github.com/FFmpeg/FFmpeg/tree/9047fa1b084f76b1b4d065af2d743df1b40dfb56
- NVIDIA codec headers: https://github.com/FFmpeg/nv-codec-headers/tree/1889e62e2d35ff7aa9baca2bceb14f053785e6f1
- Intel oneVPL: https://github.com/intel/libvpl/tree/2274efcd3672b43297ef774f332e1fed6781381c
- AOMedia libaom: https://aomedia.googlesource.com/aom/+/d9c115ce0951324dee243041ef810e27202de20f
- Reproducible Windows recipe: `scripts/build-ffmpeg-lgpl-windows.ps1`
- Reproducible macOS arm64 recipe: `scripts/build-ffmpeg-lgpl-macos.sh`
- LGPL 2.1 text: https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html

Other Rust dependencies retain their respective licenses. The distributable
must include the generated Cargo license inventory before public release.

## Microsoft Visual C++ runtime

Windows packages include `vcruntime140.dll` app-local beside `Maelstrom.exe`. The packaging
script copies it only from a trusted, operator-supplied authorized AMD64 `Microsoft.VC*.CRT`
directory
or from a locally installed Visual Studio x64 Redist directory; it is not committed to this
repository. Redistribution and use remain subject to Microsoft's applicable license terms.

- Microsoft Visual C++ Redistributable: https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170
- Microsoft Visual Studio license terms: https://visualstudio.microsoft.com/license-terms/

## Optional NVIDIA RTX VSR runtime

NVIDIA Video Effects, NGX, TensorRT, and CUDA runtime binaries are not part of the public
Maelstrom source repository and are not redistributed by this project. The source contains only
an optional dynamic loader. Users who are entitled to those components must obtain them from
NVIDIA under NVIDIA's terms and provide the local runtime through `KRAKEN_RTX_VSR_DIR` or an
`rtx-vsr` directory beside the packaged executable.

- NVIDIA Maxine / Video Effects SDK: https://developer.nvidia.com/maxine/
- NVIDIA CUDA NPP: https://docs.nvidia.com/cuda/npp/introduction.html
- NVIDIA TensorRT installation: https://docs.nvidia.com/deeplearning/tensorrt/latest/installing-tensorrt/installing.html
- Required files, code integration, and local restoration: `docs/OPTIONAL_RUNTIME_ASSETS.md`
