//! Naming every transition of a short sequence without naming every one of a
//! long one.
//!
//! A maximize on Wayland is a handful of window-system events and a handful of
//! swapchain rebuilds, and what the second does about each of the first is the
//! whole question when the window comes back black. An interactive drag is the
//! same pair sixty times a second. A line for each would drown the first in the
//! second, and a line for neither would leave nothing to read the first against.
//!
//! So every transition is a line until a budget is spent, and the budget refills
//! with the clock. A maximize costs a handful of lines against a budget that is
//! larger than that, so its sequence is complete; a drag spends the budget, and
//! the line after the gap says how many it did not name.
//!
//! Like everything in this crate it describes and does not select: the budget
//! decides whether a *line* is written and hands back nothing a product path
//! could branch on beyond the count that goes into that line.

use std::time::{Duration, Instant};

/// Lines per [`WINDOW`]. Larger than any maximize or unmaximize sequence
/// expected, and small enough that a drag is a trickle.
pub const BUDGET: u32 = 16;

/// How often the budget refills.
pub const WINDOW: Duration = Duration::from_secs(1);

/// A refilling budget of lines.
#[derive(Debug)]
pub struct LineBudget {
    window_started: Instant,
    spent: u32,
    suppressed: u32,
}

impl LineBudget {
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            window_started: now,
            spent: 0,
            suppressed: 0,
        }
    }

    /// Whether an event at `now` may be named, and if so how many were not
    /// named since the last one that was.
    pub fn admit(&mut self, now: Instant) -> Option<u32> {
        if now.saturating_duration_since(self.window_started) >= WINDOW {
            self.window_started = now;
            self.spent = 0;
        }
        if self.spent < BUDGET {
            self.spent += 1;
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A maximize is complete: every event of a sequence shorter than the
    /// budget is admitted, with nothing suppressed before it.
    #[test]
    fn a_maximize_sequence_is_named_in_full() {
        let t0 = Instant::now();
        let mut trace = LineBudget::new(t0);
        for step in 0..6 {
            assert_eq!(
                trace.admit(t0 + Duration::from_millis(step * 5)),
                Some(0),
                "event {step}"
            );
        }
    }

    /// A drag spends the budget, and the next line says what it cost.
    #[test]
    fn a_drag_is_a_trickle_and_the_gap_is_counted() {
        let t0 = Instant::now();
        let mut trace = LineBudget::new(t0);
        let mut named = 0;
        for step in 0..60u64 {
            if trace.admit(t0 + Duration::from_millis(step * 16)).is_some() {
                named += 1;
            }
        }
        assert_eq!(named, BUDGET, "a drag in one window names the budget");
        // The window turns over and the first admitted event reports the gap.
        assert_eq!(
            trace.admit(t0 + Duration::from_millis(1_200)),
            Some(60 - BUDGET)
        );
    }

    #[test]
    fn the_count_of_suppressed_events_is_reported_once() {
        let t0 = Instant::now();
        let mut trace = LineBudget::new(t0);
        for _ in 0..BUDGET + 3 {
            trace.admit(t0);
        }
        assert_eq!(trace.admit(t0 + WINDOW), Some(3));
        assert_eq!(trace.admit(t0 + WINDOW), Some(0));
    }
}
