#version 450
// Draws the guest's scanout from its own image (zero copy): the guest's memory is
// imported as the dedicated allocation of an image identical to the guest's, so
// the driver reads it the way the guest wrote it (including compression).
layout(set = 0, binding = 0) uniform sampler2D guest;

layout(push_constant) uniform Params {
    vec4 dst;   // destination rectangle in target pixels: x, y, w, h
    vec4 src;   // source rectangle in guest texels: x, y, w, h
} p;

layout(location = 0) out vec4 color;

void main() {
    vec2 t = p.src.xy + (gl_FragCoord.xy - p.dst.xy) * p.src.zw / p.dst.zw;
    color = vec4(texture(guest, t / vec2(textureSize(guest, 0))).rgb, 1.0);
}
