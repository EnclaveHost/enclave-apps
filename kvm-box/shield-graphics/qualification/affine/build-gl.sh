#!/bin/sh
set -eu
: "${OUTPUT:?set OUTPUT to a new output directory}"
src=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
mkdir "$OUTPUT"
c++ -std=c++17 -O2 -Wall -Wextra -Werror "$src/prepass-gl.cpp" -lGL -lX11 -o "$OUTPUT/prepass-gl"
cp "$src/prepass-gl.comp" "$OUTPUT/prepass-gl.comp"
