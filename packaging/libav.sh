#!/bin/sh
# Builds the ffmpeg libraries linked into the Linux binary for zero-copy
# decoding, inside the release image (packaging/Dockerfile), from the sources
# build-linux.sh fetched into $1 (the Flatpak's ffmpeg tarball and
# Vulkan-Headers at a pinned commit), and installs them into $2.
#
# Only what reads an .OSV's lens tracks onto the GPU: the MP4 reader, the
# H.265 and H.264 decoders and their Vulkan hardware decoding. Static, so the
# binary needs nothing at run time (ffmpeg loads libvulkan itself, and is
# handed wgpu's device anyway). LGPL: no --enable-gpl, no x264, so linking it
# leaves rOsmo360 under its own licences; the sources go beside the release.
set -eu
src=$1 out=$2
jobs=$(nproc)

rm -rf "$out" && mkdir -p "$out/include"
cp -r "$src/Vulkan-Headers/include/vulkan" "$src/Vulkan-Headers/include/vk_video" "$out/include/"

rm -rf "$src/build" && cp -r "$src/ffmpeg" "$src/build" && cd "$src/build"
./configure --prefix="$out" \
    --disable-everything --disable-programs --disable-doc --disable-debug --disable-autodetect \
    --disable-avdevice --disable-avfilter --disable-swscale --disable-swresample --disable-network \
    --enable-static --disable-shared --enable-pic \
    --enable-vulkan \
    --enable-decoder=hevc,h264 --enable-parser=hevc,h264 --enable-hwaccel=hevc_vulkan,h264_vulkan \
    --enable-demuxer=mov --enable-protocol=file \
    --extra-cflags="-I$out/include" >/dev/null
grep -q '^#define CONFIG_HEVC_VULKAN_HWACCEL 1' config_components.h || { echo "this ffmpeg came out without Vulkan H.265 decoding" >&2; exit 1; }
grep -q '^#define CONFIG_GPL 0' config.h || { echo "this ffmpeg came out GPL" >&2; exit 1; }
make -j"$jobs" >/dev/null && make install >/dev/null
cp COPYING.LGPLv2.1 "$out/"
