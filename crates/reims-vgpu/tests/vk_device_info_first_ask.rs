//! The guest's device-info and compute-info answers come from the device even
//! when they are the first thing this process asks the engine.
//!
//! Its own test binary on purpose: a fresh process is the one state in which
//! the Vulkan context does not exist yet, and that is the state a booting guest
//! asks in. Its driver requests device info as it loads, before anything has
//! drawn. Inside the shared unit-test process some earlier test has always
//! brought the device up, so the question could not be asked there.
//!
//! The comparison is against the same function asked again after another
//! probe has opened the device. Before the fix, the first answer was the Vulkan
//! 1.2 floor and the second was the device's, so the two differed on any host
//! above the floor. On a host exactly at the floor, or with no Vulkan device at
//! all, both answers are the floor and the test passes for the right reason.

#![cfg(feature = "backend-vulkan")]

use reims_vgpu::backend::vulkan::engine;

#[test]
fn the_first_limits_question_is_answered_by_the_device_not_the_floor() {
    let first_device_info = engine::device_info_limits();
    let first_compute = engine::compute_threadgroup_limits();

    // Any probe that opens the device; this one predates the fix.
    let _ = engine::supports_storage_image_write_without_format();

    assert_eq!(
        first_device_info,
        engine::device_info_limits(),
        "the guest keeps its first device-info answer for the whole boot"
    );
    assert_eq!(
        first_compute,
        engine::compute_threadgroup_limits(),
        "the guest keeps its first compute-info answer for the whole boot"
    );
}
