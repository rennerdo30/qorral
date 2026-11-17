#version 450
#extension GL_GOOGLE_include_directive : require
#include "particles_draw.glsl"

// Expands each particle into a camera-facing quad (6 vertices, no index buffer).

layout(std430, set = 0, binding = 0) readonly buffer Particles { Particle p[]; };

void main() {
    uint pid = uint(gl_VertexIndex) / 6u;
    emitCorner(p[pid], corners[uint(gl_VertexIndex) % 6u]);
}
