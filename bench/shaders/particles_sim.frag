#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"
#include "particles_common.glsl"

// Variant for APIs without compute shaders: one pixel per particle on 1024x1024 RGBA32F targets.
// Reads last frame's state, writes the new state into the other pair of textures.

layout(set = 0, binding = 0) uniform sampler2D posTex;
layout(set = 0, binding = 1) uniform sampler2D velTex;
layout(location = 0) in vec2 vUV;
layout(location = 0) out vec4 outPos;
layout(location = 1) out vec4 outVel;

void main() {
    ivec2 texel = ivec2(gl_FragCoord.xy);
    uint id = uint(texel.y) * 1024u + uint(texel.x);
    Particle q;
    q.pos = texelFetch(posTex, texel, 0);
    q.vel = texelFetch(velTex, texel, 0);
    q = simulate(id, q);
    outPos = q.pos;
    outVel = q.vel;
}
