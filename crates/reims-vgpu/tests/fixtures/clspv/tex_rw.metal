#include <metal_stdlib>
using namespace metal;
// read_write texture: in-place update.
kernel void tex_rw(texture2d<float, access::read_write> img [[texture(0)]],
                   uint2 gid [[thread_position_in_grid]]) {
    float4 v = img.read(gid);
    img.write(v + float4(1.0f), gid);
}
