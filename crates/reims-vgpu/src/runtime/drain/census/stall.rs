//! How long the drain worker spends blocked on the GPU, once a second.
//!
//! The question this answers is the one the asynchronous-completion work has to
//! be priced against: of the worker's wall clock, how much is it *waiting* — on
//! a ring fence, on a command buffer's own fence, or on a completion stamp that
//! could not be queued and settled the blocking way — rather than decoding and
//! recording. Waiting is the only part a completion thread can take off it.
//!
//! The stamp answers are counted beside the waits because they say whether the
//! queued rail is reachable at all: a window of `stamp_declined` and no
//! `stamp_queued` is a pathway where every stamp that owed work blocked.
//!
//! Measurement, not policy. Nothing reads these counters to decide anything.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Why the worker blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stall {
    /// Waiting a ring slot's fence so the slot can be reused or its readback
    /// consumed.
    RingWait,
    /// Waiting the fence of the command buffer just submitted.
    EntryWait,
    /// A completion stamp that could not be queued, settled by blocking on
    /// every outstanding guest read and write.
    StampSettle,
}

/// How one completion stamp was ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StampRoute {
    /// Nothing preceded it; published immediately.
    CpuReady,
    /// Handed to the completion thread behind the work it follows.
    Queued,
    /// The queued rail was required and could not take it.
    Declined,
}

const STALLS: usize = 3;
const ROUTES: usize = 3;
const WINDOW_US: u64 = 1_000_000;

static STALL_US: [AtomicU64; STALLS] = [const { AtomicU64::new(0) }; STALLS];
static STALL_N: [AtomicU64; STALLS] = [const { AtomicU64::new(0) }; STALLS];
static STAMP_N: [AtomicU64; ROUTES] = [const { AtomicU64::new(0) }; ROUTES];
static WINDOW_START_US: AtomicU64 = AtomicU64::new(0);

fn stall_index(stall: Stall) -> usize {
    match stall {
        Stall::RingWait => 0,
        Stall::EntryWait => 1,
        Stall::StampSettle => 2,
    }
}

fn route_index(route: StampRoute) -> usize {
    match route {
        StampRoute::CpuReady => 0,
        StampRoute::Queued => 1,
        StampRoute::Declined => 2,
    }
}

/// Charge the time since `started` to `stall`.
pub fn note_stall_since(stall: Stall, started: std::time::Instant) {
    let us = started.elapsed().as_micros() as u64;
    let i = stall_index(stall);
    STALL_US[i].fetch_add(us, Relaxed);
    STALL_N[i].fetch_add(1, Relaxed);
    maybe_emit();
}

/// Count one completion stamp under the route it took.
pub fn note_stamp_route(route: StampRoute) {
    STAMP_N[route_index(route)].fetch_add(1, Relaxed);
    maybe_emit();
}

fn take(cells: &[AtomicU64]) -> Vec<u64> {
    cells.iter().map(|c| c.swap(0, Relaxed)).collect()
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
    let us = take(&STALL_US);
    let n = take(&STALL_N);
    let stamps = take(&STAMP_N);
    crate::observe::off(line(window, &us, &n, &stamps));
}

fn line(window_us: u64, us: &[u64], n: &[u64], stamps: &[u64]) -> String {
    let blocked: u64 = us.iter().sum();
    format!(
        "drain_stall window_ms={} blocked_ms={} blocked_frac={:.2} ring_ms={} ring_n={} \
         entry_ms={} entry_n={} stamp_settle_ms={} stamp_settle_n={} \
         stamp_cpu_ready={} stamp_queued={} stamp_declined={}",
        window_us / 1000,
        blocked / 1000,
        blocked as f64 / window_us.max(1) as f64,
        us[0] / 1000,
        n[0],
        us[1] / 1000,
        n[1],
        us[2] / 1000,
        n[2],
        stamps[0],
        stamps[1],
        stamps[2],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line, every field one `key=value` token, in a fixed order a reader
    /// can split on.
    #[test]
    fn the_stall_line_is_one_token_per_field() {
        let l = line(
            1_000_000,
            &[250_000, 10_000, 5_000],
            &[40, 3, 2],
            &[90, 0, 12],
        );
        assert_eq!(
            l,
            "drain_stall window_ms=1000 blocked_ms=265 blocked_frac=0.27 ring_ms=250 ring_n=40 \
             entry_ms=10 entry_n=3 stamp_settle_ms=5 stamp_settle_n=2 \
             stamp_cpu_ready=90 stamp_queued=0 stamp_declined=12"
        );
        assert!(l.split(' ').skip(1).all(|t| t.split_once('=').is_some()));
    }
}
