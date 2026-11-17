// Particle color and fade as drawn (shared by particles.vert and particles_tex.vert).

struct Particle {
    vec4 pos;  // xyz position, w remaining life (s)
    vec4 vel;  // xyz velocity, w max life
};

layout(push_constant) uniform Push {
    mat4 viewProj;
    vec4 right;  // xyz camera right, w particle size
    vec4 up;
} pc;

layout(location = 0) out vec2 vLocal;
layout(location = 1) out vec3 vColor;

const vec2 corners[6] = vec2[](vec2(-1, -1), vec2(1, -1), vec2(1, 1), vec2(-1, -1), vec2(1, 1), vec2(-1, 1));

// Expands particle q into corner c of its camera-facing quad.
void emitCorner(Particle q, vec2 c) {
    float life = q.pos.w;
    float fade = clamp(life * 2.0, 0.0, 1.0) * clamp((q.vel.w - life) * 4.0, 0.0, 1.0);
    float speed = length(q.vel.xyz);
    vec3 cold = vec3(0.10, 0.30, 1.00);
    vec3 warm = vec3(1.00, 0.40, 0.08);
    vec3 col = mix(cold, warm, clamp(speed / 4.0, 0.0, 1.0)) + vec3(0.6) * clamp(speed / 6.0 - 0.6, 0.0, 1.0);
    vColor = col * fade * 0.05;
    vLocal = c;
    vec3 wp = q.pos.xyz + (pc.right.xyz * c.x + pc.up.xyz * c.y) * pc.right.w;
    gl_Position = pc.viewProj * vec4(wp, 1.0);
}
