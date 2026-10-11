#include <metal_stdlib>
using namespace metal;
// uint read and write.
kernel void tex_uint(texture2d<uint, access::read> src [[texture(0)]],
                     texture2d<uint, access::write> dst [[texture(1)]],
                     uint2 gid [[thread_position_in_grid]]) {
    uint4 v = src.read(gid);
    dst.write(v.wzyx + uint4(1), gid);
}
