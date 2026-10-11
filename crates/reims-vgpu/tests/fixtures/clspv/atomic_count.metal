#include <metal_stdlib>
using namespace metal;
// Atomics: the route does not carry them yet and must refuse by name.
kernel void atomic_count(device atomic_uint *counter [[buffer(0)]],
                         device const uint *in [[buffer(1)]],
                         uint gid [[thread_position_in_grid]]) {
    if (in[gid] & 1u) {
        atomic_fetch_add_explicit(counter, 1u, memory_order_relaxed);
    }
}
