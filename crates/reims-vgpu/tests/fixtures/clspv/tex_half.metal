#include <metal_stdlib>
using namespace metal;
// half texture write from a buffer.
kernel void tex_half(device const float4 *in [[buffer(0)]],
                     texture2d<half, access::write> dst [[texture(0)]],
                     uint2 gid [[thread_position_in_grid]]) {
    dst.write(half4(in[gid.y * 8 + gid.x]), gid);
}
