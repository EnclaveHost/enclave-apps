#!/bin/sh
# Runs in the reference image with --init, no network and no physical GPU devices.
set -eu
export HOME=/tmp/graphics-home XDG_RUNTIME_DIR=/tmp/graphics-runtime
mkdir -p "$HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json
export VK_ICD_FILENAMES="$VK_DRIVER_FILES" LP_NUM_THREADS=2
if [ -e /dev/dri ] || [ -e /dev/nvidia0 ]; then
    echo 'Unexpected physical GPU device in reference test' >&2
    exit 1
fi
exec xvfb-run -a -s '-screen 0 640x480x24 +extension GLX +render -noreset' /bin/sh -eu -c '
    vulkaninfo --summary > /evidence/vulkan-summary.txt 2>&1
    LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe glxinfo -B > /evidence/opengl-llvmpipe.txt 2>&1
    export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=zink MESA_LOADER_DRIVER_OVERRIDE=zink LIBGL_KOPPER_DRI2=true
    glxinfo -B > /evidence/opengl-zink.txt 2>&1
    vkcube --width 480 --height 360 > /evidence/vkcube.txt 2>&1 &
    cube=$!
    sleep 1
    kill -0 "$cube"
    python3 /probe/capture-reference.py /evidence/vulkan-cube.png
    kill "$cube"
    code=0
    wait "$cube" || code=$?
    test "$code" = 143
    timeout 5 glxgears -info > /evidence/glxgears-zink.txt 2>&1 &
    gears=$!
    sleep 1
    python3 /probe/capture-reference.py /evidence/opengl-gears.png
    code=0
    wait "$gears" || code=$?
    test "$code" = 124
    dpkg-query -W > /evidence/packages.txt
'
