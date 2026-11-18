#version 450
// Draws the guest's scanout straight from its memory (zero copy): the imported
// Vulkan allocation is bound as a storage buffer and read in place. Pixels are
// 32-bit little-endian x8r8g8b8 / a8r8g8b8 (bytes B, G, R, X).
layout(std430, set = 0, binding = 0) readonly buffer Guest { uint px[]; };

layout(push_constant) uniform Params {
    vec4 dst;        // destination rectangle in target pixels: x, y, w, h
    uint first;      // index of the scanout's first pixel (byte offset / 4)
    uint stride;     // pixels per row (byte stride / 4)
    uint width;      // scanout size in pixels
    uint height;
} p;

layout(location = 0) out vec4 color;

vec3 texel(ivec2 c) {
    c = clamp(c, ivec2(0), ivec2(int(p.width) - 1, int(p.height) - 1));
    vec4 v = unpackUnorm4x8(px[p.first + uint(c.y) * p.stride + uint(c.x)]);
    return v.zyx;
}

void main() {
    // Source position of this pixel's centre.
    vec2 s = (gl_FragCoord.xy - p.dst.xy) * vec2(p.width, p.height) / p.dst.zw - 0.5;
    vec2 f = fract(s);
    ivec2 c = ivec2(floor(s));
    if (p.dst.z == float(p.width) && p.dst.w == float(p.height)) {
        // 1:1, the usual case: one read, exact pixels.
        color = vec4(texel(ivec2(gl_FragCoord.xy - p.dst.xy)), 1.0);
        return;
    }
    // Scaled: bilinear from the four neighbours.
    vec3 a = mix(texel(c), texel(c + ivec2(1, 0)), f.x);
    vec3 b = mix(texel(c + ivec2(0, 1)), texel(c + ivec2(1, 1)), f.x);
    color = vec4(mix(a, b, f.y), 1.0);
}
