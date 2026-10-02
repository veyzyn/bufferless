// Draws the mouse cursor texture over the captured desktop. The viewport is
// set to the cursor's rectangle, so a single triangle covering the viewport
// is all the geometry needed (no vertex buffer, no constants).
//
// Compiled ahead of time so the app doesn't need the shader compiler DLL:
//   powershell -File shaders\build.ps1

struct VsOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

VsOut vs_main(uint id : SV_VertexID) {
    VsOut o;
    o.uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(o.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}

Texture2D cursor : register(t0);
SamplerState point_sampler : register(s0);

float4 ps_main(VsOut i) : SV_Target {
    return cursor.Sample(point_sampler, i.uv);
}
