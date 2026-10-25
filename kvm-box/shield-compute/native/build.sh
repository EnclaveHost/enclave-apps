#!/bin/sh
set -eu
JOBS=${JOBS:-3}
mkdir -p /build/src /build/out/lib/backends /build/out/bin /build/out/include
cp -a /input/llama /input/sd /input/enclave /input/shield /build/src/
LLAMA=/build/src/llama
SD=/build/src/sd
SHIELD=/build/src/shield/wasm/ggml-shielded
cmake -S "$LLAMA" -B /build/llama -G Ninja \
    -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON \
    -DGGML_BACKEND_DL=ON -DGGML_CPU_REPACK=OFF -DGGML_NATIVE=OFF -DGGML_AVX=ON -DGGML_AVX2=ON \
    -DGGML_FMA=ON -DGGML_F16C=ON -DGGML_CUDA=OFF -DGGML_VULKAN=OFF \
    -DGGML_BUILD_TESTS=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF \
    -DLLAMA_BUILD_TOOLS=ON -DLLAMA_BUILD_SERVER=ON -DLLAMA_BUILD_APP=OFF \
    -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF \
    -DCMAKE_C_FLAGS=-DGGML_MAX_NAME=128 -DCMAKE_CXX_FLAGS=-DGGML_MAX_NAME=128 \
    -DCMAKE_INSTALL_PREFIX=/build/prefix
cmake --build /build/llama --target llama-cli ggml-cpu -j "$JOBS"
cmake --install /build/llama/ggml
cmake -S "$SD" -B /build/sd -G Ninja -DCMAKE_BUILD_TYPE=Release \
    -DSD_BUILD_SHARED_LIBS=ON -DSD_USE_SYSTEM_GGML=ON -DSD_BUILD_EXAMPLES=ON \
    -DSD_WEBP=OFF -DSD_WEBM=OFF -DSD_CUDA=OFF -DSD_VULKAN=OFF \
    -DCMAKE_PREFIX_PATH=/build/prefix \
    '-DCMAKE_C_FLAGS=-DGGML_MAX_NAME=128 -I/build/prefix/include' \
    '-DCMAKE_CXX_FLAGS=-DGGML_MAX_NAME=128 -I/build/prefix/include'
cmake --build /build/sd --target sd-cli -j "$JOBS"
make -C "$SHIELD" -j "$JOBS" libggml-shielded.so shielded-run shielded-probe sd-calib \
    GGML_SRC="$LLAMA" GGML_LIB=/build/llama/bin \
    SD_SRC="$SD" SD_LIB=/build/sd/bin 'LDLIBS=-lm -lpthread -L/build/llama/bin' \
    'CXXFLAGS=-O2 -std=c++17 -Wall -Wextra -fPIC -fno-math-errno -DGGML_MAX_NAME=128'
cp -a /build/llama/bin/lib*.so* /build/sd/bin/libstable-diffusion.so* /build/out/lib/
mv /build/out/lib/libggml-cpu.so /build/out/lib/backends/
cp "$SHIELD/libggml-shielded.so" /build/out/lib/backends/
cp /build/llama/bin/llama-cli /build/sd/bin/sd-cli "$SHIELD/shielded-run" "$SHIELD/shielded-probe" "$SHIELD/sd-calib" /build/out/bin/
c++ -shared -fPIC -O2 -std=c++17 -DGGML_MAX_NAME=128 \
    -I"$LLAMA/ggml/include" -I"$LLAMA/vendor" -I"$SHIELD" /input/bootstrap.cpp \
    -L/build/llama/bin -lggml -lggml-base -lcrypto -ldl \
    -Wl,-z,relro,-z,now,-z,noexecstack -o /build/out/lib/libenclave-shield-bootstrap.so
cp -a /build/prefix/include/. /build/out/include/
cp "$LLAMA/include/llama.h" "$SD/include/stable-diffusion.h" /build/out/include/
cp /input/source-provenance.json /input/sources.lock.json /build/out/
cp "$LLAMA/LICENSE" /build/out/LICENSE.llama
cp "$SD/LICENSE" /build/out/LICENSE.sd
cp /build/src/shield/LICENSE /build/out/LICENSE.shield
for f in /build/out/bin/* /build/out/lib/*.so* /build/out/lib/backends/*.so*; do
    [ -L "$f" ] && continue
    case "$f" in
        */bin/*) patchelf --set-rpath '$ORIGIN/../lib' "$f" ;;
        */backends/*) patchelf --set-rpath '$ORIGIN/..' "$f" ;;
        *) patchelf --set-rpath '$ORIGIN' "$f" ;;
    esac
done
python3 - <<'PY'
import hashlib,json,subprocess
from pathlib import Path
root=Path('/build/out')
for path in root.rglob('*'):
    if not path.is_file() or path.is_symlink(): continue
    if path.read_bytes()[:4] != b'\x7fELF': continue
    dyn=subprocess.check_output(['readelf','-d',str(path)],text=True)
    if any(x in dyn.lower() for x in ('libcuda','libvulkan','libggml-cuda')):
        raise SystemExit('unmasked GPU dependency: '+str(path))
    segments=subprocess.check_output(['readelf','-lW',str(path)],text=True)
    if any('GNU_STACK' in line and 'RWE' in line for line in segments.splitlines()):
        raise SystemExit('executable stack: '+str(path))
files={str(f.relative_to(root)):hashlib.sha256(f.read_bytes()).hexdigest()
       for f in sorted(root.rglob('*')) if f.is_file() and f.name != 'files.sha256.json'}
(root/'files.sha256.json').write_text(json.dumps(files,indent=2)+'\n')
PY
