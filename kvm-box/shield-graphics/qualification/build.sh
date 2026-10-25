#!/bin/sh
set -eu
src=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
: "${OUTPUT:?Supply a new output directory}"
mkdir "$OUTPUT"
for shader in linear-prepass.comp prepass.vert prepass.frag; do
    glslangValidator --target-env vulkan1.1 -V "$src/$shader" -o "$OUTPUT/$shader.spv"
    spirv-val --target-env vulkan1.1 "$OUTPUT/$shader.spv"
done
c++ -std=c++17 -O2 -Wall -Wextra -Werror -Wno-missing-field-initializers \
    "$src/prepass.cpp" -o "$OUTPUT/prepass" -lvulkan
