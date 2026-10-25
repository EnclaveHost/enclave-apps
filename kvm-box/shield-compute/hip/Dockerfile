FROM enclave-kvm-shield-pocl-builder:1
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang-tools-18 libspdlog-dev libfmt-dev libffi-dev libedit-dev zlib1g-dev spirv-tools xxd \
    && dpkg-query -W > /package-versions.txt && rm -rf /var/lib/apt/lists/*
RUN ln -s /usr/bin/llvm-spirv-18 /usr/lib/llvm-18/bin/llvm-spirv
WORKDIR /build
