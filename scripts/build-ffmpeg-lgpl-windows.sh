#!/usr/bin/env bash
set -euo pipefail

# Reproducible Maelstrom Windows media spine. Run inside Debian/Ubuntu WSL.
# Required packages: mingw-w64 make nasm pkg-config git ca-certificates llvm-19.
ffmpeg_commit=9047fa1b084f76b1b4d065af2d743df1b40dfb56
nvcodec_commit=1889e62e2d35ff7aa9baca2bceb14f053785e6f1
vpl_commit=2274efcd3672b43297ef774f332e1fed6781381c

if [[ $# -ne 1 ]]; then
    echo "usage: $0 /absolute/output/prefix" >&2
    exit 2
fi
output=$1
case "$output" in
    /*) ;;
    *) echo "output prefix must be absolute" >&2; exit 2 ;;
esac
if [[ $(basename "$output") != "ffmpeg-project-8.1" ]]; then
    echo "refusing destructive replacement outside the fixed ffmpeg-project-8.1 bundle" >&2
    exit 2
fi

for tool in git make cmake ninja nasm pkg-config x86_64-w64-mingw32-gcc \
    x86_64-w64-mingw32-g++ x86_64-w64-mingw32-windres llvm-dlltool-19; do
    command -v "$tool" >/dev/null || {
        echo "missing build prerequisite: $tool" >&2
        exit 2
    }
done

build_root=$(mktemp -d /var/tmp/maelstrom-ffmpeg-8.1.XXXXXX)
cleanup() {
    local status=$?
    if [[ $status -ne 0 && -f "$build_root/ffmpeg/ffbuild/config.log" ]]; then
        echo "--- relevant FFmpeg configure failure ---" >&2
        grep -A20 -B5 -E 'ffnvcodec|cuvid requested' "$build_root/ffmpeg/ffbuild/config.log" \
            | tail -120 >&2 || true
    fi
    rm -rf -- "$build_root"
    return "$status"
}
trap cleanup EXIT
prefix="$build_root/prefix"
mkdir -p "$prefix"

fetch_commit() {
    local directory=$1
    local commit=$2
    local attempt
    for attempt in 1 2 3; do
        if git -C "$directory" fetch -q --depth 1 origin "$commit"; then
            return 0
        fi
        echo "fetch attempt $attempt failed; retrying" >&2
        sleep 2
    done
    return 1
}

git -C "$build_root" init -q nv-codec-headers
git -C "$build_root/nv-codec-headers" remote add origin https://github.com/FFmpeg/nv-codec-headers.git
fetch_commit "$build_root/nv-codec-headers" "$nvcodec_commit"
git -C "$build_root/nv-codec-headers" checkout -q --detach FETCH_HEAD
make -C "$build_root/nv-codec-headers" PREFIX="$prefix" install

# oneVPL supplies the Intel Quick Sync dispatcher and public API used by FFmpeg's QSV codecs.
git -C "$build_root" init -q libvpl
git -C "$build_root/libvpl" remote add origin https://github.com/intel/libvpl.git
fetch_commit "$build_root/libvpl" "$vpl_commit"
git -C "$build_root/libvpl" checkout -q --detach FETCH_HEAD
cmake -S "$build_root/libvpl" -B "$build_root/libvpl-build" -G Ninja \
    -DCMAKE_SYSTEM_NAME=Windows \
    -DCMAKE_C_COMPILER=x86_64-w64-mingw32-gcc \
    -DCMAKE_CXX_COMPILER=x86_64-w64-mingw32-g++ \
    -DCMAKE_RC_COMPILER=x86_64-w64-mingw32-windres \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_SHARED_LIBS=ON \
    -DBUILD_TOOLS=OFF \
    -DBUILD_PREVIEW=OFF \
    -DINSTALL_EXAMPLE_CODE=OFF >/dev/null
cmake --build "$build_root/libvpl-build" >/dev/null
cmake --install "$build_root/libvpl-build" >/dev/null
for runtime in libstdc++-6.dll libgcc_s_seh-1.dll libwinpthread-1.dll; do
    runtime_path=$(x86_64-w64-mingw32-g++ "-print-file-name=$runtime")
    if [[ ! -f "$runtime_path" ]]; then
        echo "could not locate required oneVPL runtime: $runtime" >&2
        exit 2
    fi
    cp "$runtime_path" "$prefix/bin/$runtime"
done

git -C "$build_root" init -q ffmpeg
git -C "$build_root/ffmpeg" remote add origin https://git.ffmpeg.org/ffmpeg.git
fetch_commit "$build_root/ffmpeg" "$ffmpeg_commit"
git -C "$build_root/ffmpeg" checkout -q --detach FETCH_HEAD
# The shallow commit fetch intentionally brings no remote refs. Recreate the verified release
# label locally so FFmpeg's runtime version remains human-readable as n8.1.
git -C "$build_root/ffmpeg" tag -f n8.1 "$ffmpeg_commit"

export PKG_CONFIG_PATH="$prefix/lib/pkgconfig"
cd "$build_root/ffmpeg"
./configure \
    --prefix="$prefix" \
    --target-os=mingw32 \
    --arch=x86_64 \
    --cross-prefix=x86_64-w64-mingw32- \
    --enable-cross-compile \
    --pkg-config=pkg-config \
    --enable-shared \
    --disable-static \
    --disable-debug \
    --disable-doc \
    --disable-iconv \
    --disable-zlib \
    --disable-bzlib \
    --disable-lzma \
    --enable-ffmpeg \
    --enable-ffprobe \
    --enable-ffnvcodec \
    --enable-nvdec \
    --enable-nvenc \
    --enable-cuvid \
    --enable-d3d11va \
    --enable-dxva2 \
    --enable-mediafoundation \
    --enable-libvpl \
    --extra-cflags="-I$prefix/include" \
    --extra-ldflags="-L$prefix/lib" \
    --extra-version=maelstrom-20260824
make -j"$(nproc)" >/dev/null
make install >/dev/null

# Rust's MSVC target needs COFF .lib import libraries in addition to MinGW .dll.a files.
for def in "$prefix"/lib/*.def; do
    [[ -e "$def" ]] || continue
    name=$(basename "$def" .def)
    library=${name%-*}
    llvm-dlltool-19 -m i386:x86-64 -D "$name.dll" -d "$def" -l "$prefix/lib/$library.lib"
done

rm -rf -- "$output"
mkdir -p "$(dirname "$output")"
cp -a "$prefix" "$output"
cp "$build_root/ffmpeg/COPYING.LGPLv2.1" "$output/LICENSE.txt"
cp "$build_root/libvpl/LICENSE" "$output/oneVPL-LICENSE.txt"
{
    echo "Maelstrom reproducible FFmpeg Windows build"
    echo "FFmpeg commit: $ffmpeg_commit (tag n8.1)"
    echo "nv-codec-headers commit: $nvcodec_commit (tag n12.1.14.0)"
    echo "oneVPL commit: $vpl_commit (tag v2023.4.0)"
    echo "Cross compiler: $(x86_64-w64-mingw32-gcc --version | head -1)"
    echo "NASM: $(nasm -v)"
    echo
    "$output/bin/ffmpeg.exe" -hide_banner -version 2>&1 | sed -n '1,4p' || true
} > "$output/BUILD-MANIFEST.txt"
(cd "$output" && find bin lib -maxdepth 1 -type f -print0 | sort -z | xargs -0 sha256sum) \
    > "$output/BUILD-SHA256SUMS.txt"

echo "$output"
