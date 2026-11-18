#!/bin/sh
# Compiles the shaders to the SPIR-V files the crate embeds (committed, so a
# normal build needs no shader compiler). Needs glslc (shaderc).
set -e
cd "$(dirname "$0")"
for s in fullscreen.vert guest.frag guest_image.frag compose.comp; do
    glslc --target-env=vulkan1.1 -O "$s" -o "$s.spv"
done
