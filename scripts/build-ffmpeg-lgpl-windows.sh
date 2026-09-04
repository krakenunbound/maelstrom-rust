#!/usr/bin/env bash
set -euo pipefail

# Reproducible Maelstrom Windows media spine. Run inside Debian/Ubuntu WSL.
# Required packages: mingw-w64 make nasm pkg-config git ca-certificates llvm-19.
ffmpeg_commit=9047fa1b084f76b1b4d065af2d743df1b40dfb56
nvcodec_commit=1889e62e2d35ff7aa9baca2bceb14f053785e6f1
vpl_commit=2274efcd3672b43297ef774f332e1fed6781381c
aom_commit=d9c115ce0951324dee243041ef810e27202de20f

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
show_config_on_failure=1
cleanup() {
    local status=$?
    if [[ $status -ne 0 && $show_config_on_failure -eq 1 && -f "$build_root/ffmpeg/ffbuild/config.log" ]]; then
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

# libaom is static-only: FFmpeg links it into its LGPL shared libraries, so no separate AOM DLL
# is emitted or shipped. Encoder, tests, tools, examples, and documentation stay disabled.
git -C "$build_root" init -q aom
git -C "$build_root/aom" remote add origin https://aomedia.googlesource.com/aom
fetch_commit "$build_root/aom" "$aom_commit"
git -C "$build_root/aom" checkout -q --detach FETCH_HEAD
# Record the reviewed annotated-release name against the fetched peeled commit.
git -C "$build_root/aom" tag -f v3.13.0 "$aom_commit"
cmake -S "$build_root/aom" -B "$build_root/aom-build" -G Ninja \
    -DCMAKE_SYSTEM_NAME=Windows \
    -DCMAKE_C_COMPILER=x86_64-w64-mingw32-gcc \
    -DCMAKE_CXX_COMPILER=x86_64-w64-mingw32-g++ \
    -DCMAKE_RC_COMPILER=x86_64-w64-mingw32-windres \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DAOM_TARGET_CPU=x86_64 \
    -DBUILD_SHARED_LIBS=OFF \
    -DCONFIG_PIC=1 \
    -DCONFIG_AV1_DECODER=1 \
    -DCONFIG_AV1_ENCODER=0 \
    -DENABLE_TESTS=0 \
    -DENABLE_EXAMPLES=0 \
    -DENABLE_TOOLS=0 \
    -DENABLE_DOCS=0 >/dev/null
cmake --build "$build_root/aom-build" >/dev/null
cmake --install "$build_root/aom-build" >/dev/null
[[ -f "$prefix/lib/libaom.a" ]] || { echo "libaom static archive was not installed" >&2; exit 2; }
[[ -f "$prefix/lib/pkgconfig/aom.pc" ]] || { echo "libaom pkg-config metadata was not installed" >&2; exit 2; }
if [[ -d "$prefix/bin" ]] && find "$prefix/bin" -maxdepth 1 -type f -iname 'libaom*.dll' -print -quit | grep -q .; then
    echo "decoder-only libaom build unexpectedly emitted a shared DLL" >&2
    exit 2
fi

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
    --enable-libaom \
    --disable-encoder=libaom_av1 \
    --extra-cflags="-I$prefix/include" \
    --extra-ldflags="-L$prefix/lib" \
    --extra-libs=-lwinpthread \
    --extra-version=maelstrom-20260824 >/dev/null
show_config_on_failure=0
ffmpeg_build_log="$build_root/ffmpeg-build.log"
if ! make -j"$(nproc)" >"$ffmpeg_build_log" 2>&1; then
    echo "--- FFmpeg build failure ---" >&2
    tail -120 "$ffmpeg_build_log" >&2
    exit 2
fi
if ! make install >>"$ffmpeg_build_log" 2>&1; then
    echo "--- FFmpeg install failure ---" >&2
    tail -120 "$ffmpeg_build_log" >&2
    exit 2
fi

# Rust's MSVC target needs COFF .lib import libraries in addition to MinGW .dll.a files.
for def in "$prefix"/lib/*.def; do
    [[ -e "$def" ]] || continue
    name=$(basename "$def" .def)
    library=${name%-*}
    llvm-dlltool-19 -m i386:x86-64 -D "$name.dll" -d "$def" -l "$prefix/lib/$library.lib"
done

output_parent=$(dirname "$output")
output_leaf=$(basename "$output")
staging="$output_parent/.${output_leaf}.staging-$(date +%s)-$$"
backup="$output_parent/.${output_leaf}.rollback-$(date +%s)-$$"
mkdir -p "$output_parent"
rm -rf -- "$staging"
cp -a "$prefix" "$staging"
cp "$build_root/ffmpeg/COPYING.LGPLv2.1" "$staging/LICENSE.txt"
cp "$build_root/libvpl/LICENSE" "$staging/oneVPL-LICENSE.txt"
cp "$build_root/aom/LICENSE" "$staging/libaom-LICENSE.txt"
cp "$build_root/aom/PATENTS" "$staging/libaom-PATENTS.txt"
{
    echo "Maelstrom reproducible FFmpeg Windows build"
    echo "FFmpeg commit: $ffmpeg_commit (tag n8.1)"
    echo "nv-codec-headers commit: $nvcodec_commit (tag n12.1.14.0)"
    echo "oneVPL commit: $vpl_commit (tag v2023.4.0)"
    echo "libaom commit: $aom_commit (tag v3.13.0; decoder-only static)"
    echo "Cross compiler: $(x86_64-w64-mingw32-gcc --version | head -1)"
    echo "NASM: $(nasm -v)"
    echo
    "$staging/bin/ffmpeg.exe" -hide_banner -version 2>&1 | sed -n '1,4p' || true
} > "$staging/BUILD-MANIFEST.txt"
(cd "$staging" && find bin lib -maxdepth 1 -type f -print0 | sort -z | xargs -0 sha256sum) \
    > "$staging/BUILD-SHA256SUMS.txt"
grep -q -- '--enable-libaom' "$staging/BUILD-MANIFEST.txt" || { rm -rf -- "$staging"; echo "FFmpeg build did not enable libaom" >&2; exit 2; }
decoder_inventory=$("$staging/bin/ffmpeg.exe" -hide_banner -decoders 2>&1)
grep -Eq 'libaom[-_]av1' <<<"$decoder_inventory" || {
    rm -rf -- "$staging"
    echo "staged FFmpeg bundle does not expose the libaom AV1 decoder" >&2
    exit 2
}
encoder_inventory=$("$staging/bin/ffmpeg.exe" -hide_banner -encoders 2>&1)
if grep -Eq 'libaom[-_]av1' <<<"$encoder_inventory"; then
    rm -rf -- "$staging"
    echo "staged FFmpeg bundle unexpectedly exposes the disabled libaom AV1 encoder" >&2
    exit 2
fi
if find "$staging/bin" -maxdepth 1 -type f -iname 'libaom*.dll' -print -quit | grep -q .; then
    rm -rf -- "$staging"
    echo "staged FFmpeg bundle unexpectedly contains a libaom DLL" >&2
    exit 2
fi
if [[ -e "$output" || -L "$output" ]]; then
    mv "$output" "$backup"
fi
if ! mv "$staging" "$output"; then
    if [[ -e "$backup" || -L "$backup" ]]; then
        mv "$backup" "$output" || {
            echo "failed to activate the new bundle and restore the previous bundle: $backup" >&2
            exit 2
        }
    fi
    exit 2
fi
rm -rf -- "$backup"

echo "$output"
