FROM enclave-kvm-shield-opencl-builder:1
RUN apt-get update && apt-get install -y --no-install-recommends \
    llvm-18-dev libclang-18-dev libclang-cpp18-dev libhwloc-dev libzstd-dev libxml2-dev \
    pkg-config patch \
    && dpkg-query -W > /package-versions.txt && rm -rf /var/lib/apt/lists/*
WORKDIR /build
