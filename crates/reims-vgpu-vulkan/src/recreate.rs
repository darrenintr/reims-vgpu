//! Replacing a swapchain without leaving the window with none.
//!
//! [`crate::swapchain`] decides what a swapchain *is*. This module decides when
//! one has to be built again, in what order the old one goes, and what the
//! presenter is left holding when a step fails — three questions that used to be
//! answered inline in the window's recreate function, where the only way to ask
//! them was a live window.
//!
//! # The state machine, and the one invariant it exists for
//!
//! [`Lifecycle`] holds what the window last asked for, what the live swapchain
//! was built for, and why a rebuild is owed. The invariant is
//! **`built.is_none()` implies `pending.is_some()`**: a presenter with no usable
//! swapchain always has a rebuild armed, whatever happened to arm or clear it
//! before. Every failure arm below keeps the reason that was pending, and the
//! only transition that clears one is a swapchain that was actually built. So a
//! step failing, a surface with no area, or an out-of-date report arriving while
//! another rebuild is owed can each leave the presenter *waiting*, and none of
//! them can leave it *finished without a swapchain*. That second state is what a
//! window that stays black after a maximize looks like from here.
//!
//! The caller owes the other half: a presenter that [`Lifecycle::must_rebuild`]
//! must be drawn again even when the guest publishes nothing. The window's seq
//! gate is a property of frames, not of swapchains, and a stale
//! `last_presented_seq` is exactly the thing that would otherwise stand between
//! a failed rebuild and its retry.
//!
//! # Extents are coalesced by what was built, not by what was last asked
//!
//! [`Lifecycle::request`] compares an extent with the one the live swapchain was
//! *built for*. Asking for it again is not a change, and asking for it again
//! after leaving it — a drag that returned to its start before the next frame
//! was drawn — cancels the rebuild the leaving armed, because the swapchain
//! already is what is being asked for. A burst of intermediate extents therefore
//! costs one rebuild against the last of them, and a burst that ends where it
//! began costs none.
//!
//! # Two orderings, because two window systems disagree
//!
//! [`Replacement::Transactional`] is the Vulkan replacement model: create the
//! new swapchain naming the old as `oldSwapchain`, make everything the new one
//! needs, wait for the queue, and only then adopt the new one and destroy the
//! old. Nothing is destroyed until the replacement is complete, so a failure
//! before adoption leaves the presenter able to build again from nothing.
//!
//! One consequence is not intuitive and is why the failure arms look the way they
//! do: `vkCreateSwapchainKHR` retires a non-null `oldSwapchain` **even when it
//! fails**, and a retired swapchain cannot be named as `oldSwapchain` again
//! (VUID-VkSwapchainCreateInfoKHR-oldSwapchain-01933) or acquired from. So a
//! transactional attempt that fails after the create call has consumed the old
//! swapchain, and the honest state is "no swapchain, rebuild owed" with the old
//! one destroyed behind a queue wait — not "the old one is still usable". Only a
//! failure *before* the create call, such as a surface with no area, leaves it
//! usable, and that is decided before this module is reached.
//!
//! [`Replacement::DestroyFirst`] is the ordering the window had before:
//! wait for the queue, destroy the old swapchain, create the new one with no
//! `oldSwapchain`. It exists for a `CAMetalLayer`: MoltenVK (verified against
//! v1.4.1 `MVKSwapchain.mm`) works around a Metal present-callback regression by
//! setting the layer's `drawableSize` to 1x1 when a swapchain that still has one
//! or two unpresented images is retired. With `oldSwapchain` that clobber runs
//! *after* the new swapchain has configured the layer and nothing restores the
//! size, so every later present succeeds — flagged suboptimal only — while the
//! window shows a single stretched pixel. Destroying first makes the new
//! swapchain's configuration the last write, which is the ordering that
//! workaround assumes.
//! That is a fact about a layer-backed surface and not about a driver name, so
//! [`Replacement::for_surface`] takes exactly that.

use ash::vk;

/// Why a rebuild is owed. Closed, so a log line names one of five things and a
/// seventh cannot appear without this enum growing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The presenter has just been created and has no swapchain yet.
    Init,
    /// The window's size changed from what the swapchain was built for.
    Resize,
    /// `vkAcquireNextImageKHR` said the swapchain no longer matches the surface.
    AcquireOutOfDate,
    /// `vkQueuePresentKHR` said the same.
    PresentOutOfDate,
    /// A present or acquire succeeded and reported `VK_SUBOPTIMAL_KHR`.
    Suboptimal,
}

impl Reason {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Resize => "resize",
            Self::AcquireOutOfDate => "acquire_out_of_date",
            Self::PresentOutOfDate => "present_out_of_date",
            Self::Suboptimal => "suboptimal",
        }
    }
}

/// The order a swapchain is replaced in. See the module documentation for why
/// there are two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replacement {
    /// Create the replacement naming the old swapchain, adopt it once it is
    /// whole, then destroy the old one.
    Transactional,
    /// Destroy the old swapchain before creating the replacement, with no
    /// `oldSwapchain`.
    DestroyFirst,
}

impl Replacement {
    /// The ordering for a surface, by the one property that decides it.
    ///
    /// `layer_backed` is whether the surface is a `CAMetalLayer`
    /// (`VK_EXT_metal_surface`) — the window system's own answer, read from the
    /// native handle's kind, and not a driver or platform name. Anything else
    /// replaces transactionally.
    #[must_use]
    pub const fn for_surface(layer_backed: bool) -> Self {
        if layer_backed {
            Self::DestroyFirst
        } else {
            Self::Transactional
        }
    }

    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Transactional => "transactional",
            Self::DestroyFirst => "destroy_first",
        }
    }
}

/// What an attempt did to the swapchain that was live when it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Previous {
    /// There was none.
    None,
    /// It is untouched and still the live swapchain.
    Kept,
    /// It was destroyed, or is parked to be destroyed behind the next queue
    /// wait. Either way nothing can acquire from it again.
    Gone,
    /// It was replaced by the swapchain this attempt built.
    Replaced,
}

impl Previous {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Kept => "kept",
            Self::Gone => "gone",
            Self::Replaced => "replaced",
        }
    }
}

/// The step of a replacement a failure came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Idle,
    Create,
    Prepare,
}

impl Step {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Idle => "queue_wait_idle",
            Self::Create => "create_swapchain",
            Self::Prepare => "prepare_swapchain",
        }
    }
}

/// A replacement that did not finish, and what it left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failure {
    pub step: Step,
    pub error: vk::Result,
    pub previous: Previous,
}

/// The swapchain a [`Lifecycle`] has built, and what it was built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Built {
    /// The extent the window had asked for when this was built. Compared with
    /// later requests; see the module documentation.
    for_request: vk::Extent2D,
    /// The extent the swapchain actually has, which the surface may have chosen.
    extent: vk::Extent2D,
}

/// One rebuild in progress: what [`Lifecycle::begin`] hands out and
/// [`Lifecycle::settle`] takes back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub reason: Reason,
    /// The extent the window had asked for when the attempt began.
    pub requested: vk::Extent2D,
    /// The live swapchain's extent, if there is one.
    pub from: Option<vk::Extent2D>,
}

/// How an attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    /// A swapchain of this extent is now live.
    Built { extent: vk::Extent2D },
    /// The surface has no area. Nothing was touched.
    NotReady,
    /// The replacement failed. `previous` says whether the live swapchain
    /// survived it.
    Failed { previous: Previous },
}

/// The rebuild state of one window's swapchain. See the module documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lifecycle {
    requested: vk::Extent2D,
    built: Option<Built>,
    pending: Option<Reason>,
}

fn at_least_one(extent: vk::Extent2D) -> vk::Extent2D {
    vk::Extent2D {
        width: extent.width.max(1),
        height: extent.height.max(1),
    }
}

impl Lifecycle {
    /// A presenter with nothing built, owing its first swapchain.
    #[must_use]
    pub fn new(requested: vk::Extent2D) -> Self {
        Self {
            requested: at_least_one(requested),
            built: None,
            pending: Some(Reason::Init),
        }
    }

    /// The extent the window last asked for, never smaller than 1x1.
    pub fn requested(&self) -> vk::Extent2D {
        self.requested
    }

    /// The extent of the live swapchain, if there is one.
    #[must_use]
    pub fn built_extent(&self) -> Option<vk::Extent2D> {
        self.built.map(|built| built.extent)
    }

    /// Why a rebuild is owed, if one is.
    #[must_use]
    pub fn pending(&self) -> Option<Reason> {
        self.pending
    }

    /// Whether the presenter has no swapchain it may acquire from: none built,
    /// or one built that a rebuild is owed against. The caller must draw again
    /// while this holds, published frame or not.
    #[must_use]
    pub fn must_rebuild(&self) -> bool {
        self.built.is_none() || self.pending.is_some()
    }

    /// The window now has this size. Returns whether a rebuild is owed that was
    /// not owed before.
    pub fn request(&mut self, extent: vk::Extent2D) -> bool {
        let extent = at_least_one(extent);
        self.requested = extent;
        let Some(built) = self.built else {
            // A rebuild is already owed — the invariant — and will read
            // `requested` when it runs.
            return false;
        };
        if extent != built.for_request {
            let newly = self.pending.is_none();
            // `Resize` replaces any other reason: it is the more specific one,
            // and the rebuild it arms satisfies the others too.
            self.pending = Some(Reason::Resize);
            return newly;
        }
        if self.pending == Some(Reason::Resize) {
            // Back where the swapchain already is: the rebuild that leaving
            // armed would produce an identical swapchain.
            self.pending = None;
        }
        false
    }

    /// Owe a rebuild for `reason`, unless one is owed already for a reason that
    /// is at least as informative. The first report of a kind wins, so a log line
    /// names the event that started the owing and not the last one to repeat it.
    pub fn arm(&mut self, reason: Reason) {
        if self.pending.is_none() {
            self.pending = Some(reason);
        }
    }

    /// Start a rebuild, or `None` when none is owed.
    #[must_use]
    pub fn begin(&self) -> Option<Attempt> {
        if !self.must_rebuild() {
            return None;
        }
        Some(Attempt {
            reason: self.pending.unwrap_or(Reason::Init),
            requested: self.requested,
            from: self.built_extent(),
        })
    }

    /// Record how `attempt` ended.
    pub fn settle(&mut self, attempt: Attempt, outcome: Settled) {
        match outcome {
            Settled::Built { extent } => {
                self.built = Some(Built {
                    for_request: attempt.requested,
                    extent,
                });
                // The window may have asked for something else while this was
                // built. The request is the latest, so the rebuild is owed.
                self.pending = (self.requested != attempt.requested).then_some(Reason::Resize);
            }
            Settled::NotReady => {}
            Settled::Failed { previous } => {
                if previous != Previous::Kept {
                    self.built = None;
                }
                // The reason stays. If it was somehow absent the invariant is
                // restored here rather than trusted.
                self.pending.get_or_insert(attempt.reason);
            }
        }
    }
}

/// What a replacement is done *to*. The presenter implements it over real
/// handles and the tests over integers, so the ordering and the failure arms
/// below are asserted with no device.
///
/// "Retired" means moved out of the live slot into a list that [`Self::reap`]
/// destroys; nothing here is destroyed except by `discard` and `reap`.
pub trait Swapchains {
    type Handle: Copy + Eq + std::fmt::Debug;
    /// What [`Self::prepare`] made for a swapchain, held by the replacement
    /// until the swapchain is adopted or discarded — so adopting a swapchain
    /// that was never prepared is not a call that can be written.
    type Prepared;

    /// The live swapchain, if there is one.
    fn current(&self) -> Option<Self::Handle>;
    /// `vkQueueWaitIdle`, through whoever owns the queue.
    fn idle(&mut self) -> Result<(), vk::Result>;
    /// `vkCreateSwapchainKHR` naming `old`, or no old swapchain.
    fn create(&mut self, old: Option<Self::Handle>) -> Result<Self::Handle, vk::Result>;
    /// Everything `new` needs before it can be adopted: its images and the
    /// semaphores built from them. On failure nothing it made is left behind.
    fn prepare(&mut self, new: Self::Handle) -> Result<Self::Prepared, vk::Result>;
    /// Destroy `new` and whatever [`Self::prepare`] made for it, if it got that
    /// far. `new` was never adopted, so no work can name it.
    fn discard(&mut self, new: Self::Handle, prepared: Option<Self::Prepared>);
    /// Move the live swapchain and its per-swapchain objects to the retired
    /// list. A no-op when there is none.
    fn retire_current(&mut self);
    /// Make `new` the live swapchain. Only called with the queue idle, because
    /// it gives back claims that in-flight work holds.
    fn adopt(&mut self, new: Self::Handle, prepared: Self::Prepared);
    /// Destroy every retired swapchain. Only called with the queue idle.
    fn reap(&mut self);
}

/// Replace the live swapchain, in `policy`'s order.
///
/// # Errors
///
/// [`Failure`], carrying the step and what became of the previous swapchain.
/// After a failure the presenter holds no unadopted swapchain, and
/// [`Failure::previous`] is [`Previous::Kept`] only if the live swapchain was
/// never touched.
pub fn replace<S: Swapchains>(policy: Replacement, swapchains: &mut S) -> Result<(), Failure> {
    let old = swapchains.current();
    match policy {
        Replacement::DestroyFirst => {
            // Nothing has been touched if the wait fails, so the live swapchain
            // is still the live swapchain.
            swapchains.idle().map_err(|error| Failure {
                step: Step::Idle,
                error,
                previous: if old.is_some() {
                    Previous::Kept
                } else {
                    Previous::None
                },
            })?;
            swapchains.retire_current();
            swapchains.reap();
            let previous = if old.is_some() {
                Previous::Gone
            } else {
                Previous::None
            };
            let new = swapchains.create(None).map_err(|error| Failure {
                step: Step::Create,
                error,
                previous,
            })?;
            let prepared = match swapchains.prepare(new) {
                Ok(prepared) => prepared,
                Err(error) => {
                    swapchains.discard(new, None);
                    return Err(Failure {
                        step: Step::Prepare,
                        error,
                        previous,
                    });
                }
            };
            swapchains.adopt(new, prepared);
            Ok(())
        }
        Replacement::Transactional => {
            let consumed = if old.is_some() {
                Previous::Gone
            } else {
                Previous::None
            };
            let new = match swapchains.create(old) {
                Ok(new) => new,
                Err(error) => {
                    // The create call retires `old` whether or not it succeeds,
                    // so it is not a swapchain that can be kept. Destroy it
                    // behind a wait, which is also what lets the next attempt
                    // build from nothing.
                    forfeit(swapchains);
                    return Err(Failure {
                        step: Step::Create,
                        error,
                        previous: consumed,
                    });
                }
            };
            let prepared = match swapchains.prepare(new) {
                Ok(prepared) => prepared,
                Err(error) => {
                    swapchains.discard(new, None);
                    forfeit(swapchains);
                    return Err(Failure {
                        step: Step::Prepare,
                        error,
                        previous: consumed,
                    });
                }
            };
            // The wait comes before adoption and not after, because adoption
            // gives back the graveyard claims of presents that are still running
            // until the queue is idle.
            if let Err(error) = swapchains.idle() {
                swapchains.discard(new, Some(prepared));
                // Nothing to destroy behind a failed wait: the old swapchain is
                // parked and the next successful wait reaps it.
                swapchains.retire_current();
                return Err(Failure {
                    step: Step::Idle,
                    error,
                    previous: consumed,
                });
            }
            swapchains.retire_current();
            swapchains.adopt(new, prepared);
            swapchains.reap();
            Ok(())
        }
    }
}

/// Give up the swapchain a failed transactional create consumed: retire it, and
/// destroy it if the queue can be waited on. If it cannot, it stays parked and
/// the next successful wait destroys it.
fn forfeit<S: Swapchains>(swapchains: &mut S) {
    swapchains.retire_current();
    if swapchains.idle().is_ok() {
        swapchains.reap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn extent(width: u32, height: u32) -> vk::Extent2D {
        vk::Extent2D { width, height }
    }

    /// A lifecycle that has built a swapchain for `request`.
    fn built(request: vk::Extent2D) -> Lifecycle {
        let mut lifecycle = Lifecycle::new(request);
        let attempt = lifecycle.begin().expect("a new presenter owes a swapchain");
        lifecycle.settle(attempt, Settled::Built { extent: request });
        lifecycle
    }

    #[test]
    fn a_new_presenter_owes_its_first_swapchain_and_names_it_init() {
        let lifecycle = Lifecycle::new(extent(800, 600));
        assert!(lifecycle.must_rebuild());
        assert_eq!(lifecycle.pending(), Some(Reason::Init));
        assert_eq!(lifecycle.begin().expect("owed").from, None);
    }

    #[test]
    fn a_built_swapchain_owes_nothing_until_something_arms_it() {
        let lifecycle = built(extent(800, 600));
        assert!(!lifecycle.must_rebuild());
        assert_eq!(lifecycle.begin(), None);
    }

    /// The coalescing claim, at its owner: a burst of extents is one rebuild,
    /// against the last of them.
    #[test]
    fn a_burst_of_resizes_is_one_rebuild_against_the_last_extent() {
        let mut lifecycle = built(extent(800, 600));
        assert!(lifecycle.request(extent(900, 650)));
        for step in 0..40 {
            assert!(
                !lifecycle.request(extent(900 + step, 650 + step)),
                "a rebuild already owed is not a second one"
            );
        }
        let attempt = lifecycle.begin().expect("owed");
        assert_eq!(attempt.reason, Reason::Resize);
        assert_eq!(attempt.requested, extent(939, 689));
        assert_eq!(attempt.from, Some(extent(800, 600)));
    }

    /// A drag that ends where it began does not rebuild: the swapchain is
    /// already what is being asked for.
    #[test]
    fn a_resize_back_to_the_built_extent_cancels_the_rebuild() {
        let mut lifecycle = built(extent(800, 600));
        assert!(lifecycle.request(extent(1000, 700)));
        assert!(!lifecycle.request(extent(800, 600)));
        assert!(!lifecycle.must_rebuild());
    }

    /// ...but it does not cancel a rebuild owed for a different reason: that
    /// one is about the surface, not about the size.
    #[test]
    fn a_resize_back_does_not_cancel_an_out_of_date_rebuild() {
        let mut lifecycle = built(extent(800, 600));
        lifecycle.arm(Reason::PresentOutOfDate);
        assert!(!lifecycle.request(extent(800, 600)));
        assert_eq!(lifecycle.pending(), Some(Reason::PresentOutOfDate));
        // And a size change while it is owed is named as the resize it is,
        // without counting as a new debt.
        assert!(!lifecycle.request(extent(1000, 700)));
        assert_eq!(lifecycle.pending(), Some(Reason::Resize));
    }

    #[test]
    fn asking_for_the_built_extent_again_is_not_a_change() {
        let mut lifecycle = built(extent(800, 600));
        assert!(!lifecycle.request(extent(800, 600)));
        assert!(!lifecycle.must_rebuild());
    }

    /// A window can report zero while it is being laid out. It is never a
    /// request for a swapchain with no area.
    #[test]
    fn a_zero_extent_is_never_requested() {
        let mut lifecycle = built(extent(800, 600));
        assert!(lifecycle.request(extent(0, 0)));
        assert_eq!(lifecycle.requested(), extent(1, 1));
    }

    /// The window moving again while a rebuild is running is a rebuild owed
    /// against the extent it moved to.
    #[test]
    fn a_resize_during_an_attempt_is_owed_after_it() {
        let mut lifecycle = built(extent(800, 600));
        lifecycle.request(extent(900, 650));
        let attempt = lifecycle.begin().expect("owed");
        lifecycle.request(extent(1000, 700));
        lifecycle.settle(
            attempt,
            Settled::Built {
                extent: extent(900, 650),
            },
        );
        assert_eq!(lifecycle.built_extent(), Some(extent(900, 650)));
        assert_eq!(lifecycle.pending(), Some(Reason::Resize));
        let next = lifecycle.begin().expect("owed");
        assert_eq!(next.requested, extent(1000, 700));
    }

    /// Maximize is a request and then, usually, the window system's adjusted
    /// answer. It converges: two requests, then nothing owed.
    #[test]
    fn maximize_converges_to_one_final_swapchain() {
        let mut lifecycle = built(extent(1280, 800));
        // Maximize, then the compositor trimming the decoration.
        lifecycle.request(extent(2560, 1440));
        lifecycle.request(extent(2560, 1400));
        let attempt = lifecycle.begin().expect("owed");
        assert_eq!(attempt.requested, extent(2560, 1400));
        lifecycle.settle(
            attempt,
            Settled::Built {
                extent: extent(2560, 1400),
            },
        );
        assert!(!lifecycle.must_rebuild());
        // The same size again, as a scale-factor event produces, is nothing.
        assert!(!lifecycle.request(extent(2560, 1400)));
        assert!(!lifecycle.must_rebuild());
    }

    #[test]
    fn a_surface_with_no_area_keeps_the_swapchain_and_the_debt() {
        let mut lifecycle = built(extent(800, 600));
        lifecycle.request(extent(900, 650));
        let attempt = lifecycle.begin().expect("owed");
        lifecycle.settle(attempt, Settled::NotReady);
        assert_eq!(lifecycle.built_extent(), Some(extent(800, 600)));
        assert_eq!(lifecycle.pending(), Some(Reason::Resize));
        assert!(lifecycle.must_rebuild());
    }

    /// A failure that consumed the swapchain leaves a presenter with none and a
    /// rebuild owed — the invariant — and a later success clears it.
    #[test]
    fn a_failed_replacement_leaves_a_rebuild_owed_until_one_succeeds() {
        let mut lifecycle = built(extent(800, 600));
        lifecycle.request(extent(900, 650));
        let attempt = lifecycle.begin().expect("owed");
        lifecycle.settle(
            attempt,
            Settled::Failed {
                previous: Previous::Gone,
            },
        );
        assert_eq!(lifecycle.built_extent(), None);
        assert!(lifecycle.must_rebuild());
        assert_eq!(lifecycle.pending(), Some(Reason::Resize));
        // The window changing nothing does not clear it.
        assert!(!lifecycle.request(extent(900, 650)));
        assert!(lifecycle.must_rebuild());

        let retry = lifecycle.begin().expect("still owed");
        assert_eq!(retry.from, None);
        lifecycle.settle(
            retry,
            Settled::Built {
                extent: extent(900, 650),
            },
        );
        assert!(!lifecycle.must_rebuild());
    }

    #[test]
    fn a_failure_that_kept_the_swapchain_keeps_it() {
        let mut lifecycle = built(extent(800, 600));
        lifecycle.arm(Reason::Suboptimal);
        let attempt = lifecycle.begin().expect("owed");
        lifecycle.settle(
            attempt,
            Settled::Failed {
                previous: Previous::Kept,
            },
        );
        assert_eq!(lifecycle.built_extent(), Some(extent(800, 600)));
        assert_eq!(lifecycle.pending(), Some(Reason::Suboptimal));
    }

    #[test]
    fn the_first_reason_to_arm_is_the_one_a_log_names() {
        let mut lifecycle = built(extent(800, 600));
        lifecycle.arm(Reason::AcquireOutOfDate);
        lifecycle.arm(Reason::Suboptimal);
        assert_eq!(lifecycle.pending(), Some(Reason::AcquireOutOfDate));
    }

    /// The invariant over arbitrary sequences, not only the ones written above:
    /// no operation leaves the presenter with no swapchain and nothing owed.
    #[test]
    fn no_sequence_of_events_strands_a_presenter_without_a_swapchain() {
        // A small deterministic generator; the property is over sequences and
        // does not need a crate to produce them.
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        for _ in 0..200 {
            let mut lifecycle = Lifecycle::new(extent(800, 600));
            for _ in 0..80 {
                match next(8) {
                    0 => {
                        lifecycle.request(extent(next(3) as u32 * 100, next(3) as u32 * 100));
                    }
                    1 => lifecycle.arm(Reason::AcquireOutOfDate),
                    2 => lifecycle.arm(Reason::PresentOutOfDate),
                    3 => lifecycle.arm(Reason::Suboptimal),
                    _ => {
                        if let Some(attempt) = lifecycle.begin() {
                            let outcome = match next(5) {
                                0 => Settled::NotReady,
                                1 => Settled::Failed {
                                    previous: Previous::Kept,
                                },
                                2 => Settled::Failed {
                                    previous: Previous::Gone,
                                },
                                _ => Settled::Built {
                                    extent: attempt.requested,
                                },
                            };
                            lifecycle.settle(attempt, outcome);
                        }
                    }
                }
                assert!(
                    lifecycle.built_extent().is_some() || lifecycle.pending().is_some(),
                    "no swapchain and nothing owed: {lifecycle:?}"
                );
                assert_eq!(
                    lifecycle.begin().is_some(),
                    lifecycle.must_rebuild(),
                    "begin and must_rebuild disagree: {lifecycle:?}"
                );
            }
        }
    }

    #[test]
    fn only_a_layer_backed_surface_destroys_first() {
        assert_eq!(Replacement::for_surface(true), Replacement::DestroyFirst);
        assert_eq!(Replacement::for_surface(false), Replacement::Transactional);
    }

    /// A swapchain set that records every call and can fail any step on demand.
    #[derive(Default)]
    struct Fake {
        next: u32,
        current: Option<u32>,
        prepared: BTreeSet<u32>,
        retired: Vec<u32>,
        live: BTreeSet<u32>,
        log: Vec<String>,
        fail_idle: bool,
        fail_idle_after: Option<usize>,
        idles: usize,
        fail_create: bool,
        fail_prepare: bool,
    }

    impl Fake {
        fn with_current() -> Self {
            Self {
                next: 1,
                current: Some(0),
                live: BTreeSet::from([0]),
                ..Self::default()
            }
        }
    }

    impl Swapchains for Fake {
        type Handle = u32;
        type Prepared = ();

        fn current(&self) -> Option<u32> {
            self.current
        }

        fn idle(&mut self) -> Result<(), vk::Result> {
            self.idles += 1;
            self.log.push("idle".into());
            let late = self.fail_idle_after.is_some_and(|after| self.idles > after);
            if self.fail_idle || late {
                Err(vk::Result::ERROR_DEVICE_LOST)
            } else {
                Ok(())
            }
        }

        fn create(&mut self, old: Option<u32>) -> Result<u32, vk::Result> {
            self.log.push(format!("create({old:?})"));
            if self.fail_create {
                // The driver retires the old swapchain even on failure, which
                // the production implementation cannot observe and the
                // replacement must therefore assume.
                return Err(vk::Result::ERROR_INITIALIZATION_FAILED);
            }
            let handle = self.next;
            self.next += 1;
            self.live.insert(handle);
            Ok(handle)
        }

        fn prepare(&mut self, new: u32) -> Result<(), vk::Result> {
            self.log.push(format!("prepare({new})"));
            if self.fail_prepare {
                return Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY);
            }
            self.prepared.insert(new);
            Ok(())
        }

        fn discard(&mut self, new: u32, prepared: Option<()>) {
            self.log.push(format!("discard({new})"));
            assert_eq!(
                prepared.is_some(),
                self.prepared.contains(&new),
                "a prepared swapchain is discarded with what was prepared for it"
            );
            assert!(self.live.remove(&new), "{new} destroyed twice or unknown");
            self.prepared.remove(&new);
        }

        fn retire_current(&mut self) {
            if let Some(current) = self.current.take() {
                self.log.push(format!("retire({current})"));
                self.retired.push(current);
            }
        }

        fn adopt(&mut self, new: u32, _prepared: ()) {
            self.log.push(format!("adopt({new})"));
            assert!(self.prepared.remove(&new), "{new} adopted unprepared");
            assert!(self.current.is_none(), "adopted over a live swapchain");
            self.current = Some(new);
        }

        fn reap(&mut self) {
            for handle in std::mem::take(&mut self.retired) {
                self.log.push(format!("destroy({handle})"));
                assert!(self.live.remove(&handle), "{handle} destroyed twice");
            }
        }
    }

    fn position(log: &[String], entry: &str) -> usize {
        log.iter()
            .position(|line| line == entry)
            .unwrap_or_else(|| panic!("{entry} not in {log:?}"))
    }

    /// The point of the transactional ordering: the old swapchain is named by
    /// the create call and destroyed only after the new one is adopted.
    #[test]
    fn a_transactional_replacement_destroys_the_old_swapchain_last() {
        let mut fake = Fake::with_current();
        replace(Replacement::Transactional, &mut fake).expect("replaced");
        let log = &fake.log;
        assert!(position(log, "create(Some(0))") < position(log, "prepare(1)"));
        assert!(position(log, "prepare(1)") < position(log, "adopt(1)"));
        assert!(
            position(log, "idle") < position(log, "adopt(1)"),
            "adoption gives back claims that are only safe to give back idle"
        );
        assert!(position(log, "adopt(1)") < position(log, "destroy(0)"));
        assert_eq!(fake.current, Some(1));
        assert_eq!(fake.live, BTreeSet::from([1]));
    }

    /// And the ordering MoltenVK needs, which this module must keep.
    #[test]
    fn a_destroy_first_replacement_destroys_the_old_swapchain_before_creating() {
        let mut fake = Fake::with_current();
        replace(Replacement::DestroyFirst, &mut fake).expect("replaced");
        let log = &fake.log;
        assert!(position(log, "idle") < position(log, "destroy(0)"));
        assert!(position(log, "destroy(0)") < position(log, "create(None)"));
        assert!(
            !log.iter().any(|line| line == "create(Some(0))"),
            "no oldSwapchain on this ordering: {log:?}"
        );
        assert_eq!(fake.current, Some(1));
        assert_eq!(fake.live, BTreeSet::from([1]));
    }

    #[test]
    fn the_first_swapchain_has_no_old_one_on_either_ordering() {
        for policy in [Replacement::Transactional, Replacement::DestroyFirst] {
            let mut fake = Fake::default();
            replace(policy, &mut fake).expect("built");
            assert!(fake.log.iter().any(|line| line == "create(None)"));
            assert_eq!(fake.current, Some(0));
        }
    }

    /// A transactional create that fails has consumed the old swapchain. The
    /// presenter is left with none, nothing leaked, and able to build again.
    #[test]
    fn a_failed_transactional_create_leaves_nothing_live_and_a_retry_that_works() {
        let mut fake = Fake::with_current();
        fake.fail_create = true;
        let failure = replace(Replacement::Transactional, &mut fake).expect_err("refused");
        assert_eq!(failure.step, Step::Create);
        assert_eq!(failure.previous, Previous::Gone);
        assert_eq!(fake.current, None);
        assert!(fake.live.is_empty(), "the consumed swapchain was destroyed");

        fake.fail_create = false;
        replace(Replacement::Transactional, &mut fake).expect("the retry builds from nothing");
        assert!(fake.log.iter().any(|line| line == "create(None)"));
        assert_eq!(fake.live, BTreeSet::from([fake.current.expect("live")]));
    }

    #[test]
    fn a_failed_prepare_discards_the_new_swapchain_and_never_leaks_either() {
        for policy in [Replacement::Transactional, Replacement::DestroyFirst] {
            let mut fake = Fake::with_current();
            fake.fail_prepare = true;
            let failure = replace(policy, &mut fake).expect_err("refused");
            assert_eq!(failure.step, Step::Prepare);
            assert_eq!(failure.previous, Previous::Gone);
            assert_eq!(fake.current, None);
            assert!(fake.live.is_empty(), "{policy:?} leaked: {:?}", fake.live);
            assert!(fake.prepared.is_empty());
        }
    }

    /// The destroy-first ordering's failure before anything is touched keeps the
    /// swapchain; no other failure on either ordering can.
    #[test]
    fn only_a_wait_that_fails_before_any_destroy_keeps_the_swapchain() {
        let mut fake = Fake::with_current();
        fake.fail_idle = true;
        let failure = replace(Replacement::DestroyFirst, &mut fake).expect_err("refused");
        assert_eq!(failure.step, Step::Idle);
        assert_eq!(failure.previous, Previous::Kept);
        assert_eq!(fake.current, Some(0));
        assert_eq!(fake.live, BTreeSet::from([0]));
    }

    /// A queue that cannot be waited on cannot be destroyed behind. The new
    /// swapchain is discarded, the old one is parked and not destroyed, and the
    /// next wait that succeeds reaps it.
    #[test]
    fn a_failed_wait_before_adoption_parks_the_old_swapchain_instead_of_destroying_it() {
        let mut fake = Fake::with_current();
        fake.fail_idle = true;
        let failure = replace(Replacement::Transactional, &mut fake).expect_err("refused");
        assert_eq!(failure.step, Step::Idle);
        assert_eq!(fake.current, None);
        assert!(
            fake.live.contains(&0),
            "destroyed behind a queue that could not be waited on"
        );
        assert_eq!(fake.retired, vec![0]);
        assert!(
            !fake.live.iter().any(|handle| *handle != 0),
            "the unadopted swapchain was discarded"
        );

        fake.fail_idle = false;
        replace(Replacement::Transactional, &mut fake).expect("recovers");
        assert_eq!(fake.live, BTreeSet::from([fake.current.expect("live")]));
    }

    /// Whatever fails and wherever, the replacement ends with the live set
    /// holding at most the current swapchain plus anything parked.
    #[test]
    fn no_failure_arm_leaves_an_unaccounted_swapchain() {
        for policy in [Replacement::Transactional, Replacement::DestroyFirst] {
            for mask in 0u8..8 {
                let mut fake = Fake::with_current();
                fake.fail_create = mask & 1 != 0;
                fake.fail_prepare = mask & 2 != 0;
                fake.fail_idle_after = (mask & 4 != 0).then_some(1);
                let _ = replace(policy, &mut fake);
                let accounted: BTreeSet<u32> = fake
                    .current
                    .into_iter()
                    .chain(fake.retired.iter().copied())
                    .collect();
                assert_eq!(
                    fake.live, accounted,
                    "{policy:?} mask={mask:#05b} log={:?}",
                    fake.log
                );
            }
        }
    }
}
