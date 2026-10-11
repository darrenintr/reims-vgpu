//! How the final Store of a GVA render target lands, once a second.
//!
//! A GVA Store either stays on its engine resident and owes the guest a
//! writeback debt (`deferred`), or reads the whole frame back behind a fence and
//! scatters it into guest pages on the CPU. The second is the cost an animation
//! pays per frame, and this line says which gate sent each Store there.
//!
//! Measurement, not policy. Nothing reads these counters to decide anything.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Where one GVA Store went, and why when it was not deferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GvaStoreOutcome {
    /// Stayed on the resident; a writeback debt was armed.
    Deferred,
    /// Rendered into the resident, but the debt was refused, so the resident
    /// was read back and stored synchronously.
    ArmRefused,
    /// The device cannot hold guest-visible content GPU-only.
    GpuOnlyUnavailable,
    /// No allocation identity: the span did not fully walk.
    NoAllocation,
    /// Colour 0 names no texture object to key a debt on.
    NoTextureRef,
    /// A zero row stride.
    NoStride,
    /// The resident's extent is not the attachment's.
    ExtentMismatch,
    /// The row stride is narrower than a tight row.
    StrideShort,
    /// No colour attachment, or not a GVA target.
    NotGva,
}

const N: usize = 9;
const WINDOW_US: u64 = 1_000_000;

static COUNT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static BYTES: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static WINDOW_START_US: AtomicU64 = AtomicU64::new(0);

const NAMES: [&str; N] = [
    "deferred",
    "arm_refused",
    "gpu_only_unavailable",
    "no_allocation",
    "no_texture_ref",
    "no_stride",
    "extent_mismatch",
    "stride_short",
    "not_gva",
];

fn index(outcome: GvaStoreOutcome) -> usize {
    match outcome {
        GvaStoreOutcome::Deferred => 0,
        GvaStoreOutcome::ArmRefused => 1,
        GvaStoreOutcome::GpuOnlyUnavailable => 2,
        GvaStoreOutcome::NoAllocation => 3,
        GvaStoreOutcome::NoTextureRef => 4,
        GvaStoreOutcome::NoStride => 5,
        GvaStoreOutcome::ExtentMismatch => 6,
        GvaStoreOutcome::StrideShort => 7,
        GvaStoreOutcome::NotGva => 8,
    }
}

/// Count one final GVA Store of `bytes` frame bytes under `outcome`.
pub fn note_gva_store(outcome: GvaStoreOutcome, bytes: u64) {
    let i = index(outcome);
    COUNT[i].fetch_add(1, Relaxed);
    BYTES[i].fetch_add(bytes, Relaxed);
    maybe_emit();
}

fn maybe_emit() {
    let now = crate::observe::elapsed_us();
    let start = WINDOW_START_US.load(Relaxed);
    if start == 0 {
        let _ = WINDOW_START_US.compare_exchange(0, now.max(1), Relaxed, Relaxed);
        return;
    }
    let window = now.saturating_sub(start);
    if window < WINDOW_US
        || WINDOW_START_US
            .compare_exchange(start, now.max(1), Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let counts: Vec<u64> = COUNT.iter().map(|c| c.swap(0, Relaxed)).collect();
    let bytes: Vec<u64> = BYTES.iter().map(|c| c.swap(0, Relaxed)).collect();
    crate::observe::off(line(window, &counts, &bytes));
}

fn line(window_us: u64, counts: &[u64], bytes: &[u64]) -> String {
    let mut out = format!("gva_store_census window_ms={}", window_us / 1000);
    for ((name, n), b) in NAMES.iter().zip(counts).zip(bytes) {
        out.push_str(&format!(" {name}={n}/{}kb", b / 1024));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_outcome_has_its_own_named_token() {
        let outcomes = [
            GvaStoreOutcome::Deferred,
            GvaStoreOutcome::ArmRefused,
            GvaStoreOutcome::GpuOnlyUnavailable,
            GvaStoreOutcome::NoAllocation,
            GvaStoreOutcome::NoTextureRef,
            GvaStoreOutcome::NoStride,
            GvaStoreOutcome::ExtentMismatch,
            GvaStoreOutcome::StrideShort,
            GvaStoreOutcome::NotGva,
        ];
        let mut seen: Vec<usize> = outcomes.iter().map(|o| index(*o)).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), N);
        let l = line(1_000_000, &[3, 1, 0, 0, 0, 0, 0, 0, 2], &[3072; N]);
        assert!(l.starts_with("gva_store_census window_ms=1000 deferred=3/3kb arm_refused=1/3kb"));
        assert!(l
            .split(' ')
            .all(|t| !t.is_empty() && !t.contains(char::is_whitespace)));
    }
}
