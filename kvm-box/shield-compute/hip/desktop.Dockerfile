ARG DESKTOP_IMAGE=enclave-kvm-box-desktop:0.5.0-opencl
FROM enclave-kvm-shield-hip-builder:1 AS frontend-build
COPY frontend /src
COPY hip /opt/shield/hip
WORKDIR /src
RUN make -B all ir-compiler test-opencl test-svm test-event-errors \
    'CPPFLAGS=-DSHIELD_TEST_KERNEL=\"/opt/shield/opencl/gemm_i8.cl\"' \
    && mkdir -p /build && sh /src/build-ir-reference.sh \
    && /opt/shield/hip/bin/cucc /src/test_cuda.cu -o /src/test-cuda

FROM ${DESKTOP_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends \
    llvm-18 clang-tools-18 spirv-tools g++ \
    && rm -rf /var/lib/apt/lists/* \
    && dpkg-query -W > /package-versions.txt
COPY hip /opt/shield/hip
COPY --from=frontend-build /src/runtime /opt/shield/opencl/lib
COPY client.py /shield/client.py
COPY --from=frontend-build /src/ir-compiler /opt/shield/opencl/ir-compiler
COPY --from=frontend-build /build/gemm-reference.bc /build/gemm-reference.sha256 /opt/shield/opencl/
COPY --from=frontend-build /src/test-opencl /usr/local/bin/shield-opencl-selftest
COPY --from=frontend-build /src/test-svm /usr/local/bin/shield-opencl-svm-test
COPY --from=frontend-build /src/test-event-errors /usr/local/bin/shield-opencl-event-test
COPY --from=frontend-build /src/test-cuda /usr/local/bin/shield-cuda-selftest
COPY frontend/kernel_compiler.py frontend/gemm_i8.cl frontend/gemm_i8_cuda.cu /opt/shield/opencl/
COPY frontend/test_cuda.cu /opt/shield/hip/share/examples/shield-gemm.cu
# cucc derives its include root from __file__, so a symlink breaks discovery.
RUN for tool in hipcc cucc hipconfig; do \
      printf '#!/bin/sh\nexec /opt/shield/hip/bin/%s "$@"\n' "$tool" > "/usr/local/bin/$tool"; \
      chmod 0755 "/usr/local/bin/$tool"; \
    done \
    && (test -e /usr/lib/llvm-18/bin/llvm-spirv || ln -s /usr/bin/llvm-spirv-18 /usr/lib/llvm-18/bin/llvm-spirv) \
    && ldconfig
COPY inputs.json /opt/shield/hip/share/shield-build/image-inputs.json
