#include <metal_stdlib>
using namespace metal;
struct Params { float scale; float lo; float hi; uint bias; };
// constant buffer, float math, half, and conversions both ways.
kernel void math_convert(device const float4 *in [[buffer(0)]],
                         device int4 *out [[buffer(1)]],
                         constant Params &p [[buffer(2)]],
                         uint gid [[thread_position_in_grid]]) {
    float4 v = in[gid];
    float4 r = clamp(fma(sqrt(abs(v)), float4(p.scale), float4(1.0)), p.lo, p.hi);
    half4 h = half4(r);
    out[gid] = int4(float4(h) * 16.0f) + int(p.bias);
}
