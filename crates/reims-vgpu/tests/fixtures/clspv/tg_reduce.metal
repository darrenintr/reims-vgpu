#include <metal_stdlib>
using namespace metal;
// Threadgroup memory + barrier: each group of 64 sums its inputs into out[group].
kernel void tg_reduce(device const float *in [[buffer(0)]],
                      device float *out [[buffer(1)]],
                      uint lid [[thread_position_in_threadgroup]],
                      uint group [[threadgroup_position_in_grid]],
                      uint gid [[thread_position_in_grid]]) {
    threadgroup float scratch[64];
    scratch[lid] = in[gid];
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (uint stride = 32; stride > 0; stride >>= 1) {
        if (lid < stride) {
            scratch[lid] += scratch[lid + stride];
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    if (lid == 0) {
        out[group] = scratch[0];
    }
}
