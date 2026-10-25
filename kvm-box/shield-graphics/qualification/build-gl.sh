#!/bin/sh
set -eu
: "${OUTPUT:?set OUTPUT to a new output directory}"
mkdir "$OUTPUT"
c++ -std=c++17 -O2 -Wall -Wextra -Werror /source/prepass-gl.cpp -lGL -lX11 -o "$OUTPUT/prepass-gl"
cp /source/prepass-gl.comp "$OUTPUT/prepass-gl.comp"
