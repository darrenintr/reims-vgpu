//! Counts and wall times of memory imports and CPU texture uploads. Unlike the slow-call
//! lines alone, a summary also shows a burst of individually short calls. Requested bytes are
//! the ranges passed to the calls, not measured traffic. Calls on different threads may overlap.
//! With the parent's debug target off, no clocks, counters or formatting run.

use super::{SLOW_CALL, SUMMARY_SPAN, elapsed_since, ms, now, on};
use reims_vgpu_core::platform::os::MonoTime;
use reims_vgpu_core::sync::Mutex;
use std::collections::BTreeMap;
use std::time::Duration;

static CALLS: Mutex<Calls> = Mutex::new(Calls::new());

/// Run a resource operation and, with timing enabled, record its requested range and wall time.
/// `operation` is a fixed call-site name, so the summary's storage is bounded by those sites.
pub(crate) fn call<T>(operation: &'static str, bytes: u64, call: impl FnOnce() -> T) -> T {
    if !on() {
        return call();
    }
    let start = now();
    let result = call();
    let elapsed = elapsed_since(start);
    if elapsed >= SLOW_CALL {
        log::debug!(
            target: super::TARGET,
            "{operation}: {:.1} ms on the calling thread, {bytes} requested bytes",
            ms(elapsed),
        );
    }
    let lines = CALLS.lock().add(operation, bytes, elapsed, now());
    // The logger may call back into the library: keep it outside the summary lock.
    for line in lines {
        log::debug!(target: super::TARGET, "{line}");
    }
    result
}

struct Calls {
    first: Option<MonoTime>,
    work: BTreeMap<&'static str, Work>,
}

#[derive(Default)]
struct Work {
    calls: u64,
    bytes: u64,
    total: Duration,
    longest: Duration,
}

impl Calls {
    const fn new() -> Self {
        Self {
            first: None,
            work: BTreeMap::new(),
        }
    }

    fn add(
        &mut self,
        operation: &'static str,
        bytes: u64,
        elapsed: Duration,
        at: MonoTime,
    ) -> Vec<String> {
        let work = self.work.entry(operation).or_default();
        work.calls = work.calls.saturating_add(1);
        work.bytes = work.bytes.saturating_add(bytes);
        work.total = work.total.saturating_add(elapsed);
        work.longest = work.longest.max(elapsed);
        let first = *self.first.get_or_insert(at);
        let span = at.saturating_duration_since(first);
        if span < SUMMARY_SPAN {
            return Vec::new();
        }
        self.first = None;
        std::mem::take(&mut self.work)
            .into_iter()
            .map(|(operation, work)| {
                format!(
                    "resource summary {operation}: {} calls, {} requested bytes, \
                     wall ms total/max {:.2}/{:.2} over {:.1} ms (calls may overlap)",
                    work.calls,
                    work.bytes,
                    ms(work.total),
                    ms(work.longest),
                    ms(span),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_calls_are_counted_by_operation_and_summaries_reset() {
        let mut calls = Calls::new();
        let at = |ms: u64| MonoTime::from_nanos(ms * 1_000_000);
        for i in 0..100 {
            assert!(
                calls
                    .add("import", 4096, Duration::from_millis(2), at(i))
                    .is_empty()
            );
        }
        assert!(
            calls
                .add("upload", 16, Duration::from_millis(3), at(500))
                .is_empty()
        );
        let lines = calls.add("upload", 32, Duration::from_millis(4), at(1000));
        assert_eq!(
            lines,
            [
                "resource summary import: 100 calls, 409600 requested bytes, wall ms \
                 total/max 200.00/2.00 over 1000.0 ms (calls may overlap)",
                "resource summary upload: 2 calls, 48 requested bytes, wall ms total/max \
                 7.00/4.00 over 1000.0 ms (calls may overlap)",
            ]
        );
        assert!(
            calls
                .add("import", 8, Duration::from_millis(1), at(2000))
                .is_empty()
        );
        assert!(calls.add("import", 8, Duration::ZERO, at(3000))[0].contains("2 calls, 16 "));
        assert!(calls.work.is_empty());
    }
}
