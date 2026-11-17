// Direct3D 9 (shader model 3.0) version of fullscreen.vert: one triangle covering the screen.
// SM 3.0 has no vertex index, so the backend feeds it as a float stream (TEXCOORD7). The half-pixel
// offset and the y flip follow SPIRV-Cross's SM 3.0 output, so every D3D9 shader agrees.

uniform float4 gl_HalfPixel;

struct VSOut {
    float2 vUV : TEXCOORD0;
    float4 pos : POSITION;
};

VSOut main(float vertexIndex : TEXCOORD7) {
    // Vulkan: p = vec2((i << 1) & 2, i & 2), so 0 -> (0,0), 1 -> (2,0), 2 -> (0,2)
    float2 p = float2(vertexIndex > 0.5 && vertexIndex < 1.5 ? 2.0 : 0.0, vertexIndex > 1.5 ? 2.0 : 0.0);
    VSOut o;
    o.vUV = p;
    float4 pos = float4(p * 2.0 - 1.0, 0.0, 1.0);
    pos.x = pos.x - gl_HalfPixel.x * pos.w;
    pos.y = pos.y + gl_HalfPixel.y * pos.w;
    pos.y = -pos.y;
    o.pos = pos;
    return o;
}
