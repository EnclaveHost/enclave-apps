ARG DESKTOP_IMAGE=enclave-kvm-box-desktop:0.4.0-shield
FROM enclave-kvm-shield-opencl-builder:1 AS frontend-build
COPY frontend /src
RUN make -B all test-opencl test-event-errors \
    'CPPFLAGS=-DSHIELD_TEST_KERNEL=\"/opt/shield/opencl/gemm_i8.cl\"'

FROM ${DESKTOP_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang-18 libclang-cpp18 libllvm18 libhwloc15 libzstd1 llvm-spirv-18 \
    ocl-icd-libopencl1 clinfo \
    && rm -rf /var/lib/apt/lists/* \
    && dpkg-query -W > /package-versions.txt
COPY pocl /opt/shield/pocl
COPY --from=frontend-build /src/runtime /opt/shield/opencl/lib
COPY --from=frontend-build /src/test-opencl /usr/local/bin/shield-opencl-selftest
COPY --from=frontend-build /src/test-event-errors /usr/local/bin/shield-opencl-event-test
COPY frontend/kernel_compiler.py frontend/gemm_i8.cl /opt/shield/opencl/
COPY service.py /shield/service.py
COPY approve-selftest.py /opt/shield/opencl/approve-selftest.py
RUN mkdir -p /etc/OpenCL/vendors \
    && rm -f /etc/OpenCL/vendors/*.icd \
    && printf '/opt/shield/pocl/lib/libpocl.so.2\n' > /etc/OpenCL/vendors/pocl.icd \
    && printf '/opt/shield/opencl/lib\n/opt/shield/pocl/lib\n' > /etc/ld.so.conf.d/00-shield-opencl.conf \
    && ldconfig \
    && python3 -I /opt/shield/opencl/approve-selftest.py \
    && rm /opt/shield/opencl/approve-selftest.py
COPY inputs.json /opt/shield/opencl/inputs.json
