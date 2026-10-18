#!/bin/sh
# Builds the Windows version on Linux: rosmo360.exe cross-compiled in docker
# (MinGW), with ffmpeg and ffprobe in an ffmpeg\ folder beside it, where
# rOsmo360 looks first (`ffmpeg::tool`), zipped as
# target/windows/rOsmo360-x86_64-windows.zip, with ffmpeg's sources beside it
# as rOsmo360-windows-ffmpeg-sources.tar. Unsigned.
#
# Needs docker, curl, git, python3 and network access; wine (optional) checks
# the ffmpeg. Run from anywhere: packaging/windows/build.sh [--fresh-ffmpeg]
set -eu
cd "$(dirname "$0")/../.."

# The icon (rosmo360.ico, rendered from packaging/rosmo360.svg at 16-256 px)
# goes into the .exe as a resource, compiled by MinGW's windres and linked
# in, so Explorer, the desktop and shortcuts show it.
mkdir -p target/windows
printf '1 ICON "packaging/windows/rosmo360.ico"\n' > target/windows/icon.rc
docker build -q -t rosmo360-build:windows packaging/windows >/dev/null
docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD":/src -w /src \
    -e CARGO_TARGET_DIR=/src/target/windows -e RUSTFLAGS="-C link-arg=/src/target/windows/icon.o" rosmo360-build:windows \
    sh -c 'x86_64-w64-mingw32-windres target/windows/icon.rc -O coff -o target/windows/icon.o &&
           cargo build --release --locked --target x86_64-pc-windows-gnu &&
           x86_64-w64-mingw32-strip target/windows/x86_64-pc-windows-gnu/release/rosmo360.exe'
exe=target/windows/x86_64-pc-windows-gnu/release/rosmo360.exe

# ffmpeg, built by ffmpeg.sh from sources pinned here: the Flatpak's ffmpeg
# tarball, and x264, nv-codec-headers and Vulkan-Headers at fixed commits.
# The sources are kept in target/windows/ffmpeg-src and go into
# rOsmo360-windows-ffmpeg-sources.tar beside the zip (the GPL's corresponding
# source); the build is kept unless --fresh-ffmpeg is given.
ff=target/windows/ffmpeg
srcs=target/windows/ffmpeg-src
ffmpeg_version=8.0
ffmpeg_sha256=b2751fccb6cc4c77708113cd78b561059b6fa904b24162fa0be2d60273d27b8e
pinned="x264 https://code.videolan.org/videolan/x264.git b35605ace3ddf7c1a5d67a2eb553f034aef41d55
nv-codec-headers https://github.com/FFmpeg/nv-codec-headers.git ee1c8a8cdb05f733ca661c30fd50c74f94ad6937
Vulkan-Headers https://github.com/KhronosGroup/Vulkan-Headers.git 4c8aa84a61c12cab9780fa44298617666b491d71"
if [ "${1:-}" = --fresh-ffmpeg ]; then rm -rf "$ff" "$srcs"; fi
if [ ! -f "$ff/ffmpeg.exe" ] || [ ! -f "$ff/ffprobe.exe" ]; then
    mkdir -p "$srcs/tarballs"
    tarball="$srcs/tarballs/ffmpeg-$ffmpeg_version.tar.xz"
    [ -f "$tarball" ] || curl -fL --progress-bar -o "$tarball" "https://ffmpeg.org/releases/ffmpeg-$ffmpeg_version.tar.xz"
    echo "$ffmpeg_sha256  $tarball" | sha256sum -c --quiet -
    echo "$pinned" | while read -r name url commit; do
        archive="$srcs/tarballs/$name-$commit.tar.gz"
        if [ ! -f "$archive" ]; then
            tmp=$(mktemp -d)
            git -C "$tmp" init -q && git -C "$tmp" fetch -q --depth 1 "$url" "$commit"
            git -C "$tmp" archive --format=tar.gz --prefix="$name/" -o "$PWD/$archive" "$commit"
            rm -rf "$tmp"
        fi
    done
    rm -rf "$srcs/ffmpeg" "$srcs/x264" "$srcs/nv-codec-headers" "$srcs/Vulkan-Headers"
    mkdir -p "$srcs/ffmpeg" && tar -xJf "$tarball" -C "$srcs/ffmpeg" --strip-components=1
    for archive in "$srcs"/tarballs/*.tar.gz; do tar -xzf "$archive" -C "$srcs"; done
    cp packaging/windows/ffmpeg.sh "$srcs/tarballs/"
    echo "Building ffmpeg..."
    docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD":/src -w /src rosmo360-build:windows \
        sh packaging/windows/ffmpeg.sh /src/$srcs /src/$ff
fi

# What rOsmo360 depends on: a Windows GPU decoder for the Osmo's HEVC, x264
# for MP4 export, DNxHR for the Resolve format.
if command -v wine >/dev/null; then
    run() { WINEDEBUG=-all wine "$ff/ffmpeg.exe" -hide_banner "$@" 2>/dev/null | tr -d '\r'; }
    run -hwaccels | grep -q d3d11va || echo "warning: this ffmpeg has no d3d11va"
    run -encoders | grep -q libx264 || echo "warning: this ffmpeg has no libx264: MP4 export won't work"
    run -encoders | grep -q dnxhd || echo "warning: this ffmpeg has no DNxHR: the Resolve format won't work"
    run -version | head -1
fi

# The folder to zip: the program, ffmpeg\, the licences (ours, the fonts'
# and ffmpeg's) and where ffmpeg's source is.
out=target/windows/rOsmo360
rm -rf "$out" && mkdir -p "$out/fonts"
cp "$exe" README.md LICENSE-MIT LICENSE-APACHE "$out/"
cp -r "$ff" "$out/ffmpeg"
for d in fonts/*/; do mkdir -p "$out/fonts/$(basename "$d")" && cp "$d"*.txt "$out/fonts/$(basename "$d")/"; done
cp fonts/README.md "$out/fonts/"
cat > "$out/ffmpeg/SOURCE.txt" <<EOF
ffmpeg.exe, ffprobe.exe and the DLLs beside them are FFmpeg $ffmpeg_version
(https://ffmpeg.org) with x264, licensed under the GNU GPL version 2 or later
(LICENSE.txt). rOsmo360 runs them as separate programs. The sources they were
built from, with the build script (ffmpeg.sh), are published beside this zip
as rOsmo360-windows-ffmpeg-sources.tar on each release:
https://github.com/ravelund/rOsmo360/releases
EOF
rm -f target/windows/rOsmo360-x86_64-windows.zip
(cd target/windows && python3 -m zipfile -c rOsmo360-x86_64-windows.zip rOsmo360)
tar -cf target/windows/rOsmo360-windows-ffmpeg-sources.tar -C "$srcs" --transform 's,^tarballs,rOsmo360-windows-ffmpeg-sources,' tarballs
ls -l target/windows/rOsmo360-x86_64-windows.zip target/windows/rOsmo360-windows-ffmpeg-sources.tar
