#!/bin/sh
# Builds the ffmpeg bundled with the Windows version, inside the cross-build
# image (packaging/windows/Dockerfile), from the sources build.sh fetched into
# $1 (the same ffmpeg tarball as the Flatpak, x264, nv-codec-headers and
# Vulkan-Headers at pinned commits), and installs ffmpeg.exe, ffprobe.exe and
# their DLLs into $2. Built here rather than downloaded so the sources shipped
# beside the zip are exactly what it was built from, as the GPL asks.
#
# GPL with x264 (MP4 export), never nonfree. Decoders: Direct3D 11 and DXVA2
# (any GPU vendor), NVDEC with scale_cuda (compiled by clang, which needs no
# CUDA SDK) and Vulkan (vulkan-1.dll is loaded at run time, so only headers).
# Threads and libgcc are Windows' own and static, so the DLLs need nothing
# Windows doesn't have.
set -eu
src=$1 out=$2
deps=$src/deps
host=x86_64-w64-mingw32
jobs=$(nproc)

build() { rm -rf "$src/build-$1" && cp -r "$src/$1" "$src/build-$1" && cd "$src/build-$1"; }

build nv-codec-headers
make PREFIX="$deps" install >/dev/null

build Vulkan-Headers
mkdir -p "$deps/include" && cp -r include/vulkan include/vk_video "$deps/include/"

build x264
./configure --host=$host --cross-prefix=$host- --prefix="$deps" --enable-static --disable-cli --enable-win32thread >/dev/null
make -j"$jobs" >/dev/null && make install >/dev/null

build ffmpeg
PKG_CONFIG_LIBDIR="$deps/lib/pkgconfig" ./configure --prefix="$src/install" \
    --target-os=mingw32 --arch=x86_64 --cross-prefix=$host- --enable-cross-compile --pkg-config=pkg-config \
    --enable-gpl --enable-libx264 --enable-d3d11va --enable-dxva2 --enable-vulkan \
    --enable-ffnvcodec --enable-nvdec --enable-cuda-llvm \
    --enable-shared --disable-static --enable-w32threads --disable-pthreads \
    --disable-ffplay --disable-doc --disable-debug \
    --extra-cflags="-I$deps/include" --extra-ldflags="-static-libgcc" >/dev/null
make -j"$jobs" >/dev/null && make install >/dev/null

rm -rf "$out" && mkdir -p "$out"
cp "$src/install/bin/ffmpeg.exe" "$src/install/bin/ffprobe.exe" "$src/install/bin/"*.dll "$out/"
$host-strip "$out/"*.exe "$out/"*.dll
cp COPYING.GPLv2 "$out/LICENSE.txt"
