#!/bin/sh
# Builds the Linux release binary, target/jammy/release/rosmo360, in the
# release image (Ubuntu 22.04, see the Dockerfile), with zero-copy decoding:
# ffmpeg's libraries built by libav.sh from sources pinned here and linked in
# statically. The sources are kept in target/jammy/libav-src (their tarballs
# go into rOsmo360-linux-ffmpeg-sources.tar beside the release, the LGPL's
# corresponding source); the libraries are kept unless --fresh-libav is given.
# Needs docker and network access.
set -eu
cd "$(dirname "$0")/.."
lib=target/jammy/libav
srcs=target/jammy/libav-src
# The Flatpak's and the Windows build's ffmpeg, and their Vulkan-Headers.
ffmpeg_version=8.0
ffmpeg_sha256=b2751fccb6cc4c77708113cd78b561059b6fa904b24162fa0be2d60273d27b8e
headers=4c8aa84a61c12cab9780fa44298617666b491d71

docker build -q -t rosmo360-build:jammy packaging >/dev/null
run() { docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD":/src -w /src "$@"; }

if [ "${1:-}" = --fresh-libav ]; then rm -rf "$lib" "$srcs"; fi
if [ ! -f "$lib/lib/libavcodec.a" ]; then
    mkdir -p "$srcs/tarballs"
    tarball="$srcs/tarballs/ffmpeg-$ffmpeg_version.tar.xz"
    [ -f "$tarball" ] || curl -fL --progress-bar -o "$tarball" "https://ffmpeg.org/releases/ffmpeg-$ffmpeg_version.tar.xz"
    echo "$ffmpeg_sha256  $tarball" | sha256sum -c --quiet -
    archive="$srcs/tarballs/Vulkan-Headers-$headers.tar.gz"
    if [ ! -f "$archive" ]; then
        tmp=$(mktemp -d)
        git -C "$tmp" init -q && git -C "$tmp" fetch -q --depth 1 https://github.com/KhronosGroup/Vulkan-Headers.git "$headers"
        git -C "$tmp" archive --format=tar.gz --prefix=Vulkan-Headers/ -o "$PWD/$archive" "$headers"
        rm -rf "$tmp"
    fi
    rm -rf "$srcs/ffmpeg" "$srcs/Vulkan-Headers"
    mkdir -p "$srcs/ffmpeg" && tar -xJf "$tarball" -C "$srcs/ffmpeg" --strip-components=1
    tar -xzf "$archive" -C "$srcs"
    cp packaging/libav.sh "$srcs/tarballs/"
    echo "Building ffmpeg's libraries..."
    run rosmo360-build:jammy sh packaging/libav.sh "/src/$srcs" "/src/$lib"
fi

run -e CARGO_TARGET_DIR=/src/target/jammy -e PKG_CONFIG_PATH="/src/$lib/lib/pkgconfig" -e ROSMO360_LIBAV_STATIC=1 \
    rosmo360-build:jammy cargo build --release --locked --features zero-copy
strip target/jammy/release/rosmo360
