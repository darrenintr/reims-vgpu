#include <metal_stdlib>
using namespace metal;
// 16-bit builtins and thread_index_in_threadgroup.
kernel void short_ids(device ushort *out [[buffer(0)]],
                      ushort2 lid [[thread_position_in_threadgroup]],
                      ushort2 tg [[threadgroup_position_in_grid]],
                      ushort2 tptg [[threads_per_threadgroup]],
                      uint flat [[thread_index_in_threadgroup]]) {
    uint gx = uint(tg.x) * uint(tptg.x) + uint(lid.x);
    uint gy = uint(tg.y) * uint(tptg.y) + uint(lid.y);
    out[gy * 64 + gx] = ushort(flat);
}
