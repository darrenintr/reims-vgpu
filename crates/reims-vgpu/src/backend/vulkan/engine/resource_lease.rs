//! What a serialized guest resource retains from this rail, for as long as the
//! guest keeps the resource.
//!
//! This is the Vulkan rail's half of [`crate::model::RailResourceState`]. It
//! used to be a `#[cfg(feature = "backend-vulkan")]` field *inside*
//! [`TaskResource`] itself, spelled in this module's types — which made `model`
//! depend on `backend::vulkan`, closed a cycle back through `runtime`, and on
//! the both-rails build handed the Metal arm a field it has no business
//! reading. The model now owns an opaque slot and this module owns what goes in
//! it; read [`crate::model::RailResourceState`] for why that division and not
//! another.
//!
//! Nothing about the lifetime changed. The leases still live in the resource,
//! so deleting the resource still releases the pins at that instant rather than
//! on a sweep, and a warm bind still reads the retention back without entering
//! the engine.

use std::collections::HashMap;

use super::{
    retain_resident_resource, ResidentContentBacking, ResidentResourceLease, TargetIdentity,
};
use crate::model::{RailResourceState, TaskResource};

/// Engine objects retained for one serialized resource lifetime.
///
/// A resource may own several child identities concurrently — each identity
/// includes the mapping generation, so page recycling replaces only the
/// matching identity instead of overwriting an unrelated child lease. The lease
/// owns its resident pin and its allocation classification.
#[derive(Debug, Default)]
pub struct ResidentTargets(HashMap<TargetIdentity, ResidentResourceLease>);

impl RailResourceState for ResidentTargets {}

/// Retain and classify the engine target named by this resource.
///
/// Warm binds read the resource-owned lease without entering the engine. A
/// changed identity or engine epoch releases the old lease and resolves a new
/// one; execution remains the authority for mutable content state.
pub fn resident_target_backing(
    resource: &TaskResource,
    identity: &TargetIdentity,
) -> ResidentContentBacking {
    resident_target_backing_with(resource, identity, retain_resident_resource)
}

/// What one ask did, so the census counts acquisitions and not warm binds.
///
/// A `bool` beside the backing would have said the same thing and would not
/// have said which of the two the backing came from; the warm arm is the whole
/// reason this path exists and it is the arm that must stay off the census.
enum Retention {
    /// The resource already held a live lease for this identity.
    Warm(ResidentContentBacking),
    /// A lease was resolved from the engine, or the engine had none to give.
    Acquired(ResidentContentBacking),
}

fn resident_target_backing_with(
    resource: &TaskResource,
    identity: &TargetIdentity,
    retain: impl FnOnce(&TargetIdentity) -> Option<ResidentResourceLease>,
) -> ResidentContentBacking {
    let retention = resource
        .with_rail_state(|held: &mut ResidentTargets| {
            if let Some(lease) = held.0.get(identity).filter(|lease| lease.matches(identity)) {
                return Retention::Warm(lease.backing());
            }
            // An engine reset invalidates the lease under this exact identity,
            // but another identity is usually another child resource, not a
            // replacement. A texture can own several views/surfaces
            // concurrently.
            held.0.remove(identity);
            let acquired = retain(identity);
            let backing = acquired
                .as_ref()
                .map(|lease| lease.backing())
                .unwrap_or(ResidentContentBacking::NotReady);
            if let Some(lease) = acquired {
                held.0.insert(identity.clone(), lease);
            }
            // The exception is an older generation of the same surface: its
            // pages were recycled, so its lease would pin a resident nothing can
            // ask for again for as long as the resource lives. Released after
            // the acquisition, so the new generation resolves while the old
            // resident is still held.
            let before = held.0.len();
            held.0.retain(|older, _| !identity.supersedes(older));
            for _ in held.0.len()..before {
                crate::runtime::drain::note_store_route("resident_lease_superseded");
            }
            Retention::Acquired(backing)
        })
        // `None` is another rail holding the slot, which `backend::select`'s
        // one-rail-per-process latch makes unreachable. It reads as an
        // acquisition that found nothing, because that is what it would mean:
        // this rail retains nothing for this resource, and the ask was not a
        // warm hit.
        .unwrap_or(Retention::Acquired(ResidentContentBacking::NotReady));

    match retention {
        Retention::Warm(backing) => backing,
        Retention::Acquired(backing) => {
            crate::runtime::drain::note_store_route(
                if backing != ResidentContentBacking::NotReady {
                    "resident_resource_acquired"
                } else {
                    "resident_resource_unavailable"
                },
            );
            backing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::decode::resource::ListObjectEntry;
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};

    /// These tests read process-wide census counters and advance the global
    /// resident epoch, so two of them running at once read each other's work.
    static LEASE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        LEASE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn identity(generation: u64) -> TargetIdentity {
        surface(9, 64, generation)
    }

    fn surface(id: u32, width: u32, generation: u64) -> TargetIdentity {
        TargetIdentity::Surface {
            id,
            width,
            height: 32,
            generation,
            format: crate::backend::vulkan::translate::pixel::SCANOUT_FORMAT,
        }
    }

    #[test]
    fn a_resource_retains_each_child_identity_until_the_resource_ends() {
        let _serial = serial();
        let resource = TaskResource::new(ListObjectEntry::default(), Arc::from([]));
        let first = identity(1);
        let acquisitions = Cell::new(0_u32);
        let acquired_before =
            crate::runtime::drain::census::store_route_count("resident_resource_acquired");

        let backing = resident_target_backing_with(&resource, &first, |identity| {
            acquisitions.set(acquisitions.get() + 1);
            Some(ResidentResourceLease::test_new(
                identity.clone(),
                ResidentContentBacking::GuestAllocation,
            ))
        });
        assert_eq!(backing, ResidentContentBacking::GuestAllocation);
        assert_eq!(acquisitions.get(), 1);
        assert_eq!(
            crate::runtime::drain::census::store_route_count("resident_resource_acquired"),
            acquired_before + 1
        );

        let backing = resident_target_backing_with(&resource, &first, |_| {
            panic!("a warm bind must not reacquire its live resource")
        });
        assert_eq!(backing, ResidentContentBacking::GuestAllocation);
        assert_eq!(
            crate::runtime::drain::census::store_route_count("resident_resource_acquired"),
            acquired_before + 1,
            "a warm bind must not be counted as another acquisition"
        );

        super::super::test_advance_resident_resource_epoch();
        let backing = resident_target_backing_with(&resource, &first, |identity| {
            acquisitions.set(acquisitions.get() + 1);
            Some(ResidentResourceLease::test_new(
                identity.clone(),
                ResidentContentBacking::DeviceAllocation,
            ))
        });
        assert_eq!(backing, ResidentContentBacking::DeviceAllocation);
        assert_eq!(acquisitions.get(), 2, "an engine reset reacquires once");

        let replacement = identity(2);
        let backing = resident_target_backing_with(&resource, &replacement, |identity| {
            acquisitions.set(acquisitions.get() + 1);
            Some(ResidentResourceLease::test_new(
                identity.clone(),
                ResidentContentBacking::GuestAllocation,
            ))
        });
        assert_eq!(backing, ResidentContentBacking::GuestAllocation);
        assert_eq!(
            acquisitions.get(),
            3,
            "a new mapping generation reacquires once"
        );

        let other_surface = surface(10, 64, 1);
        resident_target_backing_with(&resource, &other_surface, |identity| {
            acquisitions.set(acquisitions.get() + 1);
            Some(ResidentResourceLease::test_new(
                identity.clone(),
                ResidentContentBacking::GuestAllocation,
            ))
        });
        assert_eq!(
            resident_target_backing_with(&resource, &replacement, |_| {
                panic!("adding another surface must not evict the first one")
            }),
            ResidentContentBacking::GuestAllocation
        );
        assert_eq!(
            acquisitions.get(),
            4,
            "both child identities remain retained"
        );
        assert_eq!(
            crate::runtime::drain::census::store_route_count("resident_resource_acquired"),
            acquired_before + 4
        );

        // The synthetic leases have no registry pins behind them. Make the
        // final drop stale so it exercises the reset-safe no-op release.
        super::super::test_advance_resident_resource_epoch();
    }

    #[test]
    fn an_unavailable_target_is_counted_and_retried() {
        let _serial = serial();
        let resource = TaskResource::new(ListObjectEntry::default(), Arc::from([]));
        let unavailable_before =
            crate::runtime::drain::census::store_route_count("resident_resource_unavailable");
        let attempts = Cell::new(0_u32);

        for _ in 0..2 {
            assert_eq!(
                resident_target_backing_with(&resource, &identity(1), |_| {
                    attempts.set(attempts.get() + 1);
                    None
                }),
                ResidentContentBacking::NotReady
            );
        }
        assert_eq!(attempts.get(), 2, "an absent target must remain retryable");
        assert_eq!(
            crate::runtime::drain::census::store_route_count("resident_resource_unavailable"),
            unavailable_before + 2
        );
    }

    /// #111: a long-lived resource whose surface pages are recycled acquired a
    /// lease per generation and kept every one, so `pinned` grew with uptime on
    /// an idle desktop. A new generation of the same surface now releases the
    /// lease on the old one.
    #[test]
    fn a_new_surface_generation_releases_the_old_generations_lease() {
        let _serial = serial();
        let resource = TaskResource::new(ListObjectEntry::default(), Arc::from([]));
        let superseded_before =
            crate::runtime::drain::census::store_route_count("resident_lease_superseded");
        let acquire = |identity: &TargetIdentity| {
            Some(ResidentResourceLease::test_new(
                identity.clone(),
                ResidentContentBacking::GuestAllocation,
            ))
        };
        for generation in 1..=20 {
            resident_target_backing_with(&resource, &identity(generation), acquire);
        }
        let held = resource
            .with_rail_state(|held: &mut ResidentTargets| {
                held.0.keys().cloned().collect::<Vec<_>>()
            })
            .expect("this rail owns the slot");
        assert_eq!(
            held,
            vec![identity(20)],
            "only the current generation stays pinned"
        );
        assert_eq!(
            crate::runtime::drain::census::store_route_count("resident_lease_superseded"),
            superseded_before + 19
        );

        let reacquired = std::cell::Cell::new(false);
        resident_target_backing_with(&resource, &identity(1), |identity| {
            reacquired.set(true);
            acquire(identity)
        });
        assert!(reacquired.get(), "a superseded generation is not held warm");

        super::super::test_advance_resident_resource_epoch();
    }

    #[test]
    fn only_a_surface_generation_supersedes() {
        let _serial = serial();
        let format = crate::backend::vulkan::translate::pixel::SCANOUT_FORMAT;
        let gva = |generation| TargetIdentity::Gva {
            gva: 0x1000,
            width: 64,
            height: 32,
            generation,
            format,
        };
        assert!(identity(2).supersedes(&identity(1)));
        assert!(
            identity(2).supersedes(&surface(9, 128, 1)),
            "a generation belongs to the mapping, not to one extent of it"
        );
        assert!(
            !identity(1).supersedes(&identity(1)),
            "an identity is not its own successor"
        );
        assert!(!identity(2).supersedes(&surface(10, 64, 1)));
        assert!(
            !gva(2).supersedes(&gva(1)),
            "no GVA generation contract is established"
        );
    }
}
