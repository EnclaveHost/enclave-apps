FROM debian@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a
RUN apt-get update && apt-get install -y --no-install-recommends \
    g++ make cmake ninja-build python3 python3-numpy pocl-opencl-icd \
    ocl-icd-opencl-dev clinfo clang-19 llvm-spirv-19 nlohmann-json3-dev \
    && dpkg-query -W > /package-versions.txt && rm -rf /var/lib/apt/lists/*
ENV POCL_DEVICES=pthread POCL_MAX_PTHREAD_COUNT=2 OCL_ICD_VENDORS=/etc/OpenCL/vendors/pocl.icd
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends patchelf \
    && dpkg-query -W > /package-versions.txt && rm -rf /var/lib/apt/lists/*
