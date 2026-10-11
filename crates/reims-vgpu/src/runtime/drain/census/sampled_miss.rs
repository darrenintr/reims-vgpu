//! Why guest-gather sampled binds moved bytes, once a second.
//!
//! Each bind that misses the identity cache and gathers is named by exactly one
//! cause (`engine::pools::sampled_identity_miss_route`): the witness spent the
//! generation (`spent_*`, a compulsory miss) or a vouched image was not in the
//! cache (`lost_*`). The store-route counters carry the same names but count
//! only under `cfg(test)`; this line is the live reading the sampled-texture
//! rewrite is designed from.
//!
//! Measurement, not policy. Nothing reads these counters to decide anything.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Every name `sampled_identity_miss_route` can return, in line order.
pub(crate) const ROUTES: [&str; 16] = [
    "sampled_miss_spent_unwitnessed",
    "sampled_miss_spent_new_window",
    "sampled_miss_spent_pages_moved",
    "sampled_miss_spent_span_moved",
    "sampled_miss_spent_untracked",
    "sampled_miss_spent_arming",
    "sampled_miss_spent_no_baseline",
    "sampled_miss_spent_guest_store",
    "sampled_miss_spent_host_write",
    "sampled_miss_spent_guest_store_host_write",
    "sampled_miss_spent_audit",
    "sampled_miss_lost_no_identity",
    "sampled_miss_lost_identity_off",
    "sampled_miss_lost_evicted",
    "sampled_miss_lost_other_key",
    "sampled_miss_lost_beyond_ledger",
];

const N: usize = ROUTES.len();
const WINDOW_US: u64 = 1_000_000;

static COUNT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static BYTES: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static WINDOW_START_US: AtomicU64 = AtomicU64::new(0);

/// Count one gather of `bytes` under `route`. A name outside [`ROUTES`] is a
/// programming error the partition test rules out; it is dropped, not counted
/// somewhere it does not belong.
pub fn note_sampled_miss(route: &'static str, bytes: u64) {
    let Some(i) = ROUTES.iter().position(|r| *r == route) else {
        return;
    };
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

/// The line, with only the causes that occurred: sixteen mostly-zero tokens a
/// second would bury the two that matter.
fn line(window_us: u64, counts: &[u64], bytes: &[u64]) -> String {
    let total: u64 = counts.iter().sum();
    let mut out = format!(
        "sampled_miss_census window_ms={} gathers={total}",
        window_us / 1000
    );
    for ((name, n), b) in ROUTES.iter().zip(counts).zip(bytes) {
        if *n != 0 {
            let short = name.strip_prefix("sampled_miss_").unwrap_or(name);
            out.push_str(&format!(" {short}={n}/{}kb", b / 1024));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_names_only_the_causes_that_occurred() {
        let mut counts = [0; N];
        let mut bytes = [0; N];
        counts[1] = 900;
        bytes[1] = 900 * 4096;
        counts[13] = 3;
        bytes[13] = 3 * 2048;
        let l = line(1_000_000, &counts, &bytes);
        assert_eq!(
            l,
            "sampled_miss_census window_ms=1000 gathers=903 spent_new_window=900/3600kb lost_evicted=3/6kb"
        );
    }

    #[test]
    fn route_names_are_unique() {
        let mut names = ROUTES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), N);
    }
}
