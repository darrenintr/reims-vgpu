#include <metal_stdlib>
using namespace metal;
// sampling with a constexpr sampler.
kernel void tex_sample_const(texture2d<float, access::sample> src [[texture(0)]],
                             device float4 *out [[buffer(0)]],
                             uint2 gid [[thread_position_in_grid]]) {
    constexpr sampler s(coord::normalized, filter::nearest, address::clamp_to_edge);
    float2 uv = (float2(gid) + 0.5f) / 8.0f;
    out[gid.y * 8 + gid.x] = src.sample(s, uv, level(0));
}
