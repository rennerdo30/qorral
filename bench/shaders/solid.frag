#version 450
// One color over the whole target (the latency scene: black, white after an input).
layout(location = 0) in vec2 vUV;
layout(location = 0) out vec4 outColor;
layout(push_constant) uniform Push { vec4 color; } pc;

void main() { outColor = pc.color; }
