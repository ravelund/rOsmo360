# Build image for release binaries: Ubuntu 22.04 links against glibc 2.35, so the
# binary also runs on Debian 12, Fedora 36+ and newer Ubuntus (a build on a rolling
# distro would need its brand-new glibc).
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
# nasm and libclang are for zero-copy decoding: ffmpeg's libraries are built
# here (libav.sh) and bound with bindgen.
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates build-essential pkg-config libasound2-dev git nasm libclang-dev xz-utils && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable && chmod -R a+rwX /opt/cargo /opt/rustup
