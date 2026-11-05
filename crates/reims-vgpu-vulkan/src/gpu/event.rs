//! `id<MTLEvent>`, `id<MTLSharedEvent>`, its handle and its listener: core's `SharedEvent` over
//! the runtime's events.

use std::time::Duration;

use metal2vulkan_runtime as rt;
use reims_vgpu_core::platform::event::SharedEvent;
use reims_vgpu_core::raise::RaiseResult;

use crate::answer::plain;
use crate::platform::{Obj, VulkanPlatform};

/// `id<MTLEvent>`.
pub type VulkanEvent = Obj<rt::Event>;

/// `id<MTLSharedEvent>`.
pub type VulkanSharedEvent = Obj<rt::SharedEvent>;

/// `MTLSharedEventHandle *`: in this process only, as the runtime's handle is.
pub type VulkanSharedEventHandle = Obj<rt::SharedEventHandle>;

/// `MTLSharedEventListener *`.
pub type VulkanSharedEventListener = Obj<rt::SharedEventListener>;

impl SharedEvent<VulkanPlatform> for VulkanSharedEvent {
    /// A handle of this process: the runtime's handles open in the process that made them.
    fn new_shared_event_handle(&self) -> Option<VulkanSharedEventHandle> {
        Some(Obj::new(plain(|| {
            rt::SharedEvent::new_shared_event_handle(self)
        })))
    }

    /// The value becomes at least `signaled_value`; the runtime refuses no value.
    fn set_signaled_value(&self, signaled_value: u64) -> RaiseResult<()> {
        // evset: event, value: the guest's or the device's host set
        reims_vgpu_core::trace::event(
            "evset",
            &[crate::gpu::command::event_id(self), signaled_value],
        );
        plain(|| rt::SharedEvent::set_signaled_value(self, signaled_value));
        Ok(())
    }

    /// Blocks up to `milliseconds`.
    fn wait_until_signaled_value(&self, value: u64, milliseconds: u64) -> bool {
        plain(|| {
            rt::SharedEvent::wait_until_signaled_value(
                self,
                value,
                Duration::from_millis(milliseconds),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use reims_vgpu_core::platform::device::GpuDevice;

    use super::*;
    use crate::gpu::device::tests::dev;

    /// setSignaledValue: sets the value; waitUntilSignaledValue:timeoutMS: is YES for a reached
    /// value and NO after the timeout for an unreached one
    #[test]
    fn signal_and_wait() {
        let e = dev().new_shared_event().unwrap();
        SharedEvent::set_signaled_value(&e, 3).unwrap();
        assert_eq!(e.signaled_value(), 3);
        assert!(SharedEvent::wait_until_signaled_value(&e, 3, 0));
        assert!(SharedEvent::wait_until_signaled_value(&e, 2, 0));
        assert!(!SharedEvent::wait_until_signaled_value(&e, 4, 10));
    }
}
