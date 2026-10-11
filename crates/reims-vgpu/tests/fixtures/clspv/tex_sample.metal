#include <metal_stdlib>
using namespace metal;
// sampling with an argument sampler.
kernel void tex_sample(texture2d<float, access::sample> src [[texture(0)]],
                       sampler s [[sampler(0)]],
                       device float4 *out [[buffer(0)]],
                       uint2 gid [[thread_position_in_grid]]) {
    float2 uv = (float2(gid) + 0.5f) / 8.0f;
    out[gid.y * 8 + gid.x] = src.sample(s, uv, level(0));
}
