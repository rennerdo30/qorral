#version 450
#extension GL_GOOGLE_include_directive : require
#include "particles_draw.glsl"

// Variant for APIs without compute shaders: the particle state comes from two 1024x1024 RGBA32F
// textures (position and velocity) written by particles_sim.frag.

layout(set = 0, binding = 0) uniform sampler2D posTex;
layout(set = 0, binding = 1) uniform sampler2D velTex;

void main() {
    uint pid = uint(gl_VertexIndex) / 6u;
    ivec2 texel = ivec2(int(pid & 1023u), int(pid >> 10u));
    Particle q;
    q.pos = texelFetch(posTex, texel, 0);
    q.vel = texelFetch(velTex, texel, 0);
    emitCorner(q, corners[uint(gl_VertexIndex) % 6u]);
}
