#include <metal_stdlib>
using namespace metal;
// read and write 2D float textures, and the size query.
kernel void tex_copy(texture2d<float, access::read> src [[texture(0)]],
                     texture2d<float, access::write> dst [[texture(1)]],
                     device uint *size [[buffer(0)]],
                     uint2 gid [[thread_position_in_grid]]) {
    float4 v = src.read(gid);
    dst.write(v * 2.0f + float4(0.25f), gid);
    if (gid.x == 0 && gid.y == 0) {
        size[0] = src.get_width();
        size[1] = src.get_height();
    }
}
