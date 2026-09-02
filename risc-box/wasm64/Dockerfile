# The wasm64 toolchain as an image: prepare-toolchain.sh in a container, so
# a build box without the host prerequisites can produce risc-box64.wasm.
#
#   docker build -f wasm64/Dockerfile -t risc-box-w64 .
#   docker run --rm -v "$PWD:/src" -w /src risc-box-w64 sh wasm64/build.sh
#
# The image holds only the toolchain ($W64 = /opt/w64) and a nightly with
# rust-src + wasm32-wasip2; the app is built from the mounted checkout.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl xz-utils git ninja-build python3 patch build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*
# CMake by pinned release (wasi-libc floor 3.26; bookworm apt has 3.25)
ARG CMAKE_VERSION=3.31.6
RUN curl -fsSL -o /tmp/cmake.tgz "https://github.com/Kitware/CMake/releases/download/v${CMAKE_VERSION}/cmake-${CMAKE_VERSION}-linux-x86_64.tar.gz" \
 && tar -xzf /tmp/cmake.tgz -C /usr/local --strip-components=1 && rm /tmp/cmake.tgz
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
ARG RUST_NIGHTLY=nightly-2026-08-07
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain "${RUST_NIGHTLY}" --profile minimal \
      -c rust-src -t wasm32-wasip2 \
 && rustup toolchain install stable --profile minimal
COPY wasm64/ /opt/w64-src/wasm64/
# the scripts take the toolchain name from RUST_TC (default "nightly"); pin it
ENV W64=/opt/w64 RUST_TC=${RUST_NIGHTLY}
RUN sh /opt/w64-src/wasm64/prepare-toolchain.sh && rm -rf /opt/w64/dl /opt/w64/wasm-tools-src/target /opt/w64/wac-src/target
