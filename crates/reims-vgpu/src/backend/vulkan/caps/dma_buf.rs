//! Whether a guest RAMBlock that lives in a file can reach this device as a
//! dma-buf instead of a host-pointer import.
//!
//! # Why a second import primitive exists
//!
//! `VK_EXT_external_memory_host` is the portable rail, and on amdgpu it is a
//! userptr buffer object. Measured on an RX 7600 / RADV (Mesa 26.2) with no VM
//! involved: an empty submit costs 0.057 ms; with one 2 GiB userptr import alive
//! in the process — referenced or not — every submit costs 27 ms; and once one
//! page of it is write-protected (what KVM dirty logging does to guest RAM all
//! the time) the next submit re-faults the whole buffer, 457 ms. The same 2 GiB
//! as a memfd turned into a dma-buf by `/dev/udmabuf` and imported with
//! `VK_EXT_external_memory_dma_buf` costs 0.046 ms a submit and 0.050 ms after
//! the same write-protect: its pages are pinned once, so nothing revalidates.
//!
//! So this is not a second policy for the same thing. It is the primitive that
//! does not make every submit pay for the guest's whole memory, available only
//! where the RAMBlock is a file and the device can import one.
//!
//! An operator override may narrow this rail and never widen it:
//! `REIMS_VGPU_GUEST_IMPORT=off` turns it off together with the host-pointer
//! import, because both are the same guest-memory import seen through two
//! handle types.

use ash::vk;

/// The answer for one physical device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DmaBufCaps {
    /// Both extensions are advertised and a dma-buf can back a buffer with the
    /// usage a guest-RAM import is created with.
    available: bool,
}

impl DmaBufCaps {
    pub fn is_available(self) -> bool {
        self.available
    }

    /// The extension strings this answer requires, and only on a device where it
    /// is available — asking for one a device does not advertise fails
    /// `vkCreateDevice` outright.
    pub fn required_extensions(self) -> Vec<*const i8> {
        if self.available {
            vec![
                ash::khr::external_memory_fd::NAME.as_ptr(),
                ash::ext::external_memory_dma_buf::NAME.as_ptr(),
            ]
        } else {
            Vec::new()
        }
    }
}

/// Resolve dma-buf importability against one physical device.
///
/// # Safety
///
/// `pd` must belong to `instance`.
pub unsafe fn query(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    has_extension: &dyn Fn(&std::ffi::CStr) -> bool,
) -> DmaBufCaps {
    if crate::config::read(crate::config::GUEST_IMPORT).0 == crate::config::Switch::Off {
        return DmaBufCaps::default();
    }
    if !has_extension(ash::khr::external_memory_fd::NAME)
        || !has_extension(ash::ext::external_memory_dma_buf::NAME)
    {
        return DmaBufCaps::default();
    }
    let info = vk::PhysicalDeviceExternalBufferInfo::default()
        .usage(super::host_pointer::GUEST_IMPORT_USAGE)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut props = vk::ExternalBufferProperties::default();
    unsafe { instance.get_physical_device_external_buffer_properties(pd, &info, &mut props) };
    DmaBufCaps {
        available: props
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device without the rail names no extension, so it still gets a device.
    #[test]
    fn only_an_available_rail_asks_for_its_extensions() {
        assert!(DmaBufCaps::default().required_extensions().is_empty());
        assert_eq!(
            DmaBufCaps { available: true }.required_extensions().len(),
            2
        );
    }
}
