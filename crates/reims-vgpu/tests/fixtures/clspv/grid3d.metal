#include <metal_stdlib>
using namespace metal;
// thread_position_in_grid as uint3, threads_per_grid: out[linear] = linear index.
kernel void grid3d(device uint *out [[buffer(0)]],
                   uint3 gid [[thread_position_in_grid]],
                   uint3 grid [[threads_per_grid]]) {
    uint linear = gid.x + grid.x * (gid.y + grid.y * gid.z);
    out[linear] = linear;
}
