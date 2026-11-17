// Direct3D 9 (shader model 3.0) version of particles_sim.frag: one pixel per particle on two
// 1024x1024 RGBA32F targets (position, velocity). Same flow field, attractors, damping and
// lifetimes as particles_common.glsl; SM 3.0 has no integer math, so the respawn randomness uses a
// float hash of the particle's texel instead of the integer PCG hash.

cbuffer Push
{
    float4 pc_header : packoffset(c0);      // time, dt, count, mode
    float4 pc_attractor[3] : packoffset(c1);  // xyz position, w strength
};

uniform sampler2D posTex : register(s0);
uniform sampler2D velTex : register(s1);

struct PSOut {
    float4 pos : COLOR0;
    float4 vel : COLOR1;
};

float hash13(float3 p3) {
    p3 = frac(p3 * 0.1031);
    p3 += dot(p3, p3.zyx + 31.32);
    return frac((p3.x + p3.y) * p3.z);
}

// Float replacement for uhash(pcgHash(...)): texel plus a salt, mixed twice.
float rnd(float2 texel, float salt) {
    return hash13(float3(texel, salt * 7.31 + 0.5) + hash13(float3(texel.yx, salt + 11.7)) * 113.0);
}

float4 noised(float3 x) {
    float3 i = floor(x);
    float3 w = frac(x);
    float3 u = w * w * w * (w * (w * 6.0 - 15.0) + 10.0);
    float3 du = 30.0 * w * w * (w * (w - 2.0) + 1.0);
    float a = hash13(i + float3(0, 0, 0));
    float b = hash13(i + float3(1, 0, 0));
    float c = hash13(i + float3(0, 1, 0));
    float d = hash13(i + float3(1, 1, 0));
    float e = hash13(i + float3(0, 0, 1));
    float f = hash13(i + float3(1, 0, 1));
    float g = hash13(i + float3(0, 1, 1));
    float h = hash13(i + float3(1, 1, 1));
    float k0 = a, k1 = b - a, k2 = c - a, k3 = e - a, k4 = a - b - c + d;
    float k5 = a - c - e + g, k6 = a - b - e + f, k7 = -a + b + c - d + e - f - g + h;
    float v = k0 + k1 * u.x + k2 * u.y + k3 * u.z + k4 * u.x * u.y + k5 * u.y * u.z + k6 * u.z * u.x + k7 * u.x * u.y * u.z;
    float3 grad = du * float3(k1 + k4 * u.y + k6 * u.z + k7 * u.y * u.z,
                              k2 + k5 * u.z + k4 * u.x + k7 * u.z * u.x,
                              k3 + k6 * u.x + k5 * u.y + k7 * u.x * u.y);
    return float4(2.0 * v - 1.0, 2.0 * grad);
}

float3 curl(float3 p) {
    float3 g1 = noised(p).yzw;
    float3 g2 = noised(p + float3(31.4, 17.2, 5.9)).yzw;
    float3 g3 = noised(p + float3(-11.7, 43.1, 23.3)).yzw;
    return float3(g3.y - g2.z, g1.z - g3.x, g2.x - g1.y);
}

float3 randomDir(float2 texel, float salt) {
    float z = rnd(texel, salt) * 2.0 - 1.0;
    float a = rnd(texel, salt + 0.37) * 6.2831853;
    float r = sqrt(max(0.0, 1.0 - z * z));
    return float3(r * cos(a), z, r * sin(a));
}

PSOut spawn(float2 texel, float salt, bool initial) {
    PSOut q;
    float maxLife = 2.0 + 6.0 * rnd(texel, salt + 1.0);
    if (initial) {
        q.pos.xyz = randomDir(texel, salt + 2.0) * (4.0 * pow(rnd(texel, salt + 3.0), 0.33));
        q.pos.w = maxLife * rnd(texel, salt + 4.0);
        q.vel = float4(0.0, 0.0, 0.0, maxLife);
    } else {
        float k = floor(rnd(texel, salt + 5.0) * 2.9999);
        float3 dir = randomDir(texel, salt + 6.0);
        float3 center = k < 0.5 ? pc_attractor[0].xyz : (k < 1.5 ? pc_attractor[1].xyz : pc_attractor[2].xyz);
        q.pos.xyz = center + dir * (0.3 + 1.2 * rnd(texel, salt + 7.0));
        q.pos.w = maxLife;
        q.vel = float4(cross(dir, float3(0.0, 1.0, 0.0)) * 1.5, maxLife);
    }
    return q;
}

PSOut main(float2 vpos : VPOS) {
    float2 texel = floor(vpos);
    float2 uv = (texel + 0.5) / 1024.0;
    float time = pc_header.x, dt = pc_header.y;
    if (pc_header.w < 0.5) return spawn(texel, 0.0, true);
    float4 qpos = tex2Dlod(posTex, float4(uv, 0.0, 0.0));
    float4 qvel = tex2Dlod(velTex, float4(uv, 0.0, 0.0));
    float3 pos = qpos.xyz;
    float3 vel = qvel.xyz;

    float3 flow = curl(pos * 0.45 + float3(0.0, time * 0.07, 0.0)) + 0.5 * curl(pos * 1.1 - time * 0.05);
    float3 acc = flow * 3.0;
    for (int i = 0; i < 3; i++) {
        float3 d = pc_attractor[i].xyz - pos;
        float r2 = dot(d, d) + 0.35;
        acc += d * (pc_attractor[i].w / (r2 * sqrt(r2)));
    }
    vel += acc * dt;
    vel *= exp(-0.5 * dt);
    float speed = length(vel);
    if (speed > 6.0) vel *= 6.0 / speed;
    pos += vel * dt;
    float life = qpos.w - dt;

    PSOut o;
    if (life <= 0.0 || dot(pos, pos) > 400.0) {
        o = spawn(texel, frac(time * 0.6180339) * 97.0 + 1.0, false);
    } else {
        o.pos = float4(pos, life);
        o.vel = float4(vel, qvel.w);
    }
    return o;
}
