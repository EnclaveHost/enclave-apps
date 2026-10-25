# Development reference renderer. CPU execution is deliberate; this image is
# not evidence of masked hardware graphics and is not a published release.
ARG BASE_IMAGE
FROM ${BASE_IMAGE}
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    mesa-vulkan-drivers mesa-utils vulkan-tools xvfb xauth libjson-c5 \
    && mkdir -p /opt/shield/graphics-reference \
    && dpkg-query -W > /opt/shield/graphics-reference/packages.txt \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*
