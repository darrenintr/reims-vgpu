//! What first-use pipeline creation costs, and what could have been done about it.
//!
//! `pipe_create_us` on the tranche ledger says how long the drain thread spent
//! inside `vkCreate{Graphics,Compute}Pipelines`; it cannot say *why* that was long.
//! A 966 ms tranche of which 933 ms was thirteen creates is two very different
//! stories depending on whether those thirteen were served from the persisted
//! `VkPipelineCache` or were genuine driver compiles, and the repairs differ: a
//! warm cache that missed wants its persistence or its bound fixed, a genuine
//! compile wants to happen before the draw that needs it. Nothing measured which.
//!
//! # Three answers, one line a second
//!
//! - **Hit or compile.** `VK_EXT_pipeline_creation_feedback` reports, per create,
//!   whether the driver found the pipeline in the application's cache
//!   (`APPLICATION_PIPELINE_CACHE_HIT`). Chained only on a device that advertises
//!   the extension; a create without it is counted as `unknown`, never as a hit.
//!   The decision is the driver's own, so no duration threshold stands in for it.
//! - **How long.** Wall time around the create call itself (breadcrumb and cache
//!   persist excluded) — total, worst, and how many exceeded 50 and 100 ms, which
//!   are the hitch thresholds the performance target is written in.
//! - **How much runway an early compile would have had.** For a create the
//!   driver had to compile, the time between the guest declaring the pipeline
//!   object and the first draw that used it. A compile that starts at
//!   declaration can only hide a hitch if that runway is longer than the compile;
//!   if most first uses follow their declaration by a millisecond, an early
//!   compile has nothing to hide behind and the lever is the cache, not the
//!   schedule.
//!
//! Measuring is all it does. Nothing here is read back to decide a draw.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Hitch thresholds the 60 Hz target is written against.
const OVER_50_MS_US: u64 = 50_000;
const OVER_100_MS_US: u64 = 100_000;

/// Lead-time bucket upper bounds in microseconds: <1 ms, <10 ms, <50 ms, <250 ms,
/// and everything beyond. A compile is tens of milliseconds, so the buckets sit
/// on both sides of it.
const LEAD_BOUNDS_US: [u64; 4] = [1_000, 10_000, 50_000, 250_000];

/// What the driver said about one create.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Served {
    /// The driver found it in the application's pipeline cache.
    CacheHit,
    /// The driver compiled it.
    Compiled,
    /// No feedback was requested or the driver did not mark it valid.
    Unknown,
}

impl Served {
    /// Decode the creation-feedback flags the driver filled in.
    ///
    /// `valid` is `VK_PIPELINE_CREATION_FEEDBACK_VALID_BIT`: the spec lets a
    /// driver leave the whole struct unwritten, and an unwritten struct is not
    /// evidence of a compile.
    pub(crate) fn from_feedback(valid: bool, application_cache_hit: bool) -> Self {
        match (valid, application_cache_hit) {
            (false, _) => Self::Unknown,
            (true, true) => Self::CacheHit,
            (true, false) => Self::Compiled,
        }
    }
}

#[derive(Debug, Default)]
struct Class {
    n: AtomicU64,
    us: AtomicU64,
    max_us: AtomicU64,
}

impl Class {
    fn add(&self, us: u64) {
        self.n.fetch_add(1, Relaxed);
        self.us.fetch_add(us, Relaxed);
        self.max_us.fetch_max(us, Relaxed);
    }

    fn take(&self) -> (u64, u64, u64) {
        (
            self.n.swap(0, Relaxed),
            self.us.swap(0, Relaxed),
            self.max_us.swap(0, Relaxed),
        )
    }
}

/// One second's creates.
#[derive(Debug, Default)]
pub(crate) struct PipeCensus {
    hit: Class,
    compiled: Class,
    unknown: Class,
    over_50: AtomicU64,
    over_100: AtomicU64,
    /// Compiles by declaration-to-first-use lead; the last slot is "no lead":
    /// the draw carried no pipeline-object identity (compute, or a path that
    /// builds its pipeline without one).
    lead: [AtomicU64; LEAD_BOUNDS_US.len() + 2],
}

fn lead_slot(lead_us: Option<u64>) -> usize {
    match lead_us {
        None => LEAD_BOUNDS_US.len() + 1,
        Some(us) => LEAD_BOUNDS_US
            .iter()
            .position(|bound| us < *bound)
            .unwrap_or(LEAD_BOUNDS_US.len()),
    }
}

impl PipeCensus {
    /// One create finished. `lead_us` is the declaration-to-first-use interval
    /// when this create is the first use of a declared pipeline object.
    pub(crate) fn note(&self, served: Served, create_us: u64, lead_us: Option<u64>) {
        match served {
            Served::CacheHit => self.hit.add(create_us),
            Served::Compiled => {
                self.compiled.add(create_us);
                self.lead[lead_slot(lead_us)].fetch_add(1, Relaxed);
            }
            Served::Unknown => self.unknown.add(create_us),
        }
        if create_us >= OVER_50_MS_US {
            self.over_50.fetch_add(1, Relaxed);
        }
        if create_us >= OVER_100_MS_US {
            self.over_100.fetch_add(1, Relaxed);
        }
    }

    /// The second's line, zeroing the tallies, or `None` when nothing was created.
    pub(crate) fn take_line(&self, feedback_available: bool) -> Option<String> {
        let (hit_n, hit_us, hit_max) = self.hit.take();
        let (compiled_n, compiled_us, compiled_max) = self.compiled.take();
        let (unknown_n, unknown_us, unknown_max) = self.unknown.take();
        let over_50 = self.over_50.swap(0, Relaxed);
        let over_100 = self.over_100.swap(0, Relaxed);
        let mut lead = [0u64; LEAD_BOUNDS_US.len() + 2];
        for (slot, tally) in lead.iter_mut().zip(&self.lead) {
            *slot = tally.swap(0, Relaxed);
        }
        if hit_n + compiled_n + unknown_n == 0 {
            return None;
        }
        Some(format!(
            "pipe_create feedback={} hit_n={hit_n} hit_us={hit_us} hit_max_us={hit_max} \
             compiled_n={compiled_n} compiled_us={compiled_us} compiled_max_us={compiled_max} \
             unknown_n={unknown_n} unknown_us={unknown_us} unknown_max_us={unknown_max} \
             over_50ms={over_50} over_100ms={over_100} \
             compiled_lead_lt1ms={} compiled_lead_lt10ms={} compiled_lead_lt50ms={} \
             compiled_lead_lt250ms={} compiled_lead_ge250ms={} compiled_lead_none={}",
            u8::from(feedback_available),
            lead[0],
            lead[1],
            lead[2],
            lead[3],
            lead[4],
            lead[5],
        ))
    }
}

/// The process's census. Written by the drain thread inside the engine lock and
/// swapped to zero by the maintenance heartbeat.
pub(crate) fn census() -> &'static PipeCensus {
    static CENSUS: std::sync::OnceLock<PipeCensus> = std::sync::OnceLock::new();
    CENSUS.get_or_init(PipeCensus::default)
}

/// Emit the line at most once per census interval, from the heartbeat.
pub(crate) fn note_levels(now_ms: u64, feedback_available: bool) {
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    if !crate::runtime::released_pages::claim_census_interval(&LAST_MS, now_ms) {
        return;
    }
    if let Some(line) = census().take_line(feedback_available) {
        crate::observe::off(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(line: &str, name: &str) -> u64 {
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} missing from {line}"))
            .parse()
            .unwrap()
    }

    /// An unwritten feedback struct is not a compile, and a hit is only a hit
    /// when the driver said it was valid.
    #[test]
    fn feedback_decodes_to_the_drivers_own_answer() {
        assert_eq!(Served::from_feedback(false, true), Served::Unknown);
        assert_eq!(Served::from_feedback(false, false), Served::Unknown);
        assert_eq!(Served::from_feedback(true, true), Served::CacheHit);
        assert_eq!(Served::from_feedback(true, false), Served::Compiled);
    }

    /// The cold-run shape: thirteen compiles that sum to ~933 ms, one of them a
    /// hitch of its own, beside cache hits that are cheap.
    #[test]
    fn a_cold_burst_reads_as_compiles_and_names_its_worst() {
        let c = PipeCensus::default();
        for _ in 0..12 {
            c.note(Served::Compiled, 60_000, Some(300));
        }
        c.note(Served::Compiled, 213_000, Some(400_000));
        for _ in 0..40 {
            c.note(Served::CacheHit, 600, Some(2_000));
        }
        let line = c.take_line(true).expect("something was created");
        assert_eq!(field(&line, "compiled_n"), 13);
        assert_eq!(field(&line, "compiled_us"), 12 * 60_000 + 213_000);
        assert_eq!(field(&line, "compiled_max_us"), 213_000);
        assert_eq!(field(&line, "hit_n"), 40);
        assert_eq!(field(&line, "hit_max_us"), 600);
        assert_eq!(field(&line, "over_50ms"), 13);
        assert_eq!(field(&line, "over_100ms"), 1);
        // Twelve first uses followed their declaration by under a millisecond:
        // an early compile has no runway there. One had 400 ms.
        assert_eq!(field(&line, "compiled_lead_lt1ms"), 12);
        assert_eq!(field(&line, "compiled_lead_ge250ms"), 1);
        // A hit does not count towards the runway question.
        assert_eq!(
            field(&line, "compiled_lead_lt10ms")
                + field(&line, "compiled_lead_lt50ms")
                + field(&line, "compiled_lead_lt250ms"),
            0
        );
    }

    #[test]
    fn lead_buckets_have_exclusive_upper_bounds() {
        assert_eq!(lead_slot(Some(0)), 0);
        assert_eq!(lead_slot(Some(999)), 0);
        assert_eq!(lead_slot(Some(1_000)), 1);
        assert_eq!(lead_slot(Some(9_999)), 1);
        assert_eq!(lead_slot(Some(10_000)), 2);
        assert_eq!(lead_slot(Some(49_999)), 2);
        assert_eq!(lead_slot(Some(50_000)), 3);
        assert_eq!(lead_slot(Some(249_999)), 3);
        assert_eq!(lead_slot(Some(250_000)), 4);
        assert_eq!(lead_slot(Some(u64::MAX)), 4);
        assert_eq!(lead_slot(None), 5);
    }

    /// Without the extension every create is `unknown` — never silently a hit —
    /// and the line says feedback was unavailable so a reader does not mistake
    /// zero hits for a cold cache.
    #[test]
    fn a_host_without_feedback_reports_unknown_not_hits() {
        let c = PipeCensus::default();
        c.note(Served::Unknown, 70_000, None);
        c.note(Served::Unknown, 500, None);
        let line = c.take_line(false).unwrap();
        assert_eq!(field(&line, "feedback"), 0);
        assert_eq!(field(&line, "unknown_n"), 2);
        assert_eq!(field(&line, "hit_n") + field(&line, "compiled_n"), 0);
        assert_eq!(field(&line, "over_50ms"), 1);
    }

    #[test]
    fn a_quiet_second_emits_nothing_and_the_tallies_reset() {
        let c = PipeCensus::default();
        assert!(c.take_line(true).is_none());
        c.note(Served::Compiled, 10, None);
        assert!(c.take_line(true).is_some());
        assert!(c.take_line(true).is_none(), "the swap zeroed the second");
    }
}
