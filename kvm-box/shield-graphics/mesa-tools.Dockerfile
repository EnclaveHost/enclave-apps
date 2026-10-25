ARG BASE_IMAGE
FROM ${BASE_IMAGE}
USER root
RUN apt-get update && apt-get install -y --no-install-recommends \
 build-essential meson ninja-build pkg-config python3-mako python3-yaml python3-packaging \
 bison flex llvm-19-dev libclang-19-dev libdrm-dev libexpat1-dev libelf-dev \
 libx11-dev libx11-xcb-dev libxcb-keysyms1-dev libxext-dev libxfixes-dev libxxf86vm-dev libxrandr-dev libxshmfence-dev \
 libxcb-glx0-dev libxcb-dri2-0-dev libxcb-dri3-dev libxcb-present-dev libxcb-sync-dev \
 libxcb-randr0-dev libxcb-xfixes0-dev libxcb-shm0-dev libglvnd-dev libzstd-dev \
 libjson-c-dev vulkan-validationlayers zlib1g-dev glslang-tools libvulkan-dev libwayland-dev wayland-protocols \
 && rm -rf /var/lib/apt/lists/*
