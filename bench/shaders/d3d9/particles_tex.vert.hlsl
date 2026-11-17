// Direct3D 9 (shader model 3.0) version of particles_tex.vert: expands each particle into a
// camera-facing quad. The vertex index comes as a float stream (TEXCOORD7); the particle state is
// read from the RGBA32F position and velocity textures with vertex texture fetch.

cbuffer Push
{
    row_major float4x4 pc_viewProj : packoffset(c0);
    float4 pc_right : packoffset(c4);  // xyz camera right, w particle size
    float4 pc_up : packoffset(c5);
};

uniform float4 gl_HalfPixel;
uniform sampler2D posTex : register(s0);
uniform sampler2D velTex : register(s1);

struct VSOut {
    float2 vLocal : TEXCOORD0;
    float3 vColor : TEXCOORD1;
    float4 pos : POSITION;
};

VSOut main(float vertexIndex : TEXCOORD7) {
    float pid = floor(vertexIndex / 6.0 + 1e-4);
    float corner = vertexIndex - pid * 6.0;
    // corners: (-1,-1) (1,-1) (1,1) (-1,-1) (1,1) (-1,1)
    float2 c;
    c.x = (corner > 0.5 && corner < 2.5) || (corner > 3.5 && corner < 4.5) ? 1.0 : -1.0;
    c.y = (corner > 1.5 && corner < 2.5) || corner > 3.5 ? 1.0 : -1.0;

    float row = floor(pid / 1024.0 + 1e-6);
    float2 uv = (float2(pid - row * 1024.0, row) + 0.5) / 1024.0;
    float4 qpos = tex2Dlod(posTex, float4(uv, 0.0, 0.0));
    float4 qvel = tex2Dlod(velTex, float4(uv, 0.0, 0.0));

    float life = qpos.w;
    float fade = saturate(life * 2.0) * saturate((qvel.w - life) * 4.0);
    float speed = length(qvel.xyz);
    float3 cold = float3(0.10, 0.30, 1.00);
    float3 warm = float3(1.00, 0.40, 0.08);
    float3 col = lerp(cold, warm, saturate(speed / 4.0)) + 0.6 * saturate(speed / 6.0 - 0.6);

    VSOut o;
    o.vColor = col * fade * 0.05;
    o.vLocal = c;
    float3 wp = qpos.xyz + (pc_right.xyz * c.x + pc_up.xyz * c.y) * pc_right.w;
    float4 pos = mul(float4(wp, 1.0), pc_viewProj);
    pos.x = pos.x - gl_HalfPixel.x * pos.w;
    pos.y = pos.y + gl_HalfPixel.y * pos.w;
    pos.y = -pos.y;
    o.pos = pos;
    return o;
}
