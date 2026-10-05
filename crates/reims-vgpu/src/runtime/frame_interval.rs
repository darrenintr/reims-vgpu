//! The distribution of the gaps between presented frames, with the guest's own
//! silence taken out.
//!
//! `host_window_cadence` reports `present_hz` and `offered_hz`: means over a
//! second. A mean cannot separate a steady 60 Hz from 55 frames at 14 ms and one
//! 120 ms stall, and it counts an idle desktop as a slow one. Neither is what a
//! 60 Hz (let alone 120 Hz) target is written against. This reports what it is
//! written against: the interval between consecutive presents, as percentiles,
//! the worst, and how many crossed 16.7, 33.3, 50 and 100 ms.
//!
//! # Whose gap was it
//!
//! A long gap is one of two things, and only one is a performance failure.
//!
//! - The **guest produced nothing**: the drain worker sat on its condvar waiting
//!   for a doorbell. No amount of host work shortens that gap.
//! - The **host was late**: the worker was in a tranche, waiting for the device
//!   lock, or running its sweeps, or a frame was offered and refused because the
//!   queue or the swapchain was busy.
//!
//! Every gap below [`ACTIVE_BELOW_US`] is counted as it is: a guest producing at
//! 20 frames a second or better is not idle. At or above it a gap is the host's
//! only when the drain worker spent at least half of it **not** waiting for the
//! guest, or a present was refused as busy inside it. Otherwise it is an *idle
//! gap*: counted and its worst reported, but kept out of the percentiles and the
//! hitch counts, so a quiet desktop reads as quiet and not as slow.
//!
//! This is a classification of a measurement, and it is stated here so a reader
//! can disagree with it. It misattributes in one direction that matters: a stall
//! that happens entirely outside the drain worker with no busy refusal (a driver
//! blocking the window thread inside `vkQueuePresentKHR`, say) reads as idle.
//! `idle_max_ms` is what makes that visible — an idle gap with a large maximum
//! on a second the guest was animating is worth a look.
//!
//! Measuring is all it does. Nothing here is read back to decide a present.
//!
//! # Where a 120 Hz workload would be capped — the cadence chain
//!
//! Audited from the code; each stage is tagged with whether this crate can
//! measure it and where. Nothing below is a claim about a live guest.
//!
//! | stage | what bounds it | evidence |
//! |---|---|---|
//! | guest compositor period | `fRefreshPeriod`, a kernel constant defaulting to 1/60 s and replaced only when the framebuffer publishes pixel clock/count; the guest driver clears the timing-valid bit, so on the macOS 13 x86 rail it reads one of two latches (60 Hz paced, or 0 and free-running) | `drain::census` `VBL_REPORT_EARLY` doc. Not generalised to arm64: check `ioreg -l \| grep IOFBCurrentPixel` in that guest |
//! | VBL delivery | `DISPLAY_VBL_MIN_INTERVAL_US` = 1e6 / `DISPLAY_REFRESH_HZ` (8 333 µs), phase-locked to a µs grid; polls arrive every 4 ms, so deliveries alternate 8 and 12 ms around the 8.33 mean | `claim_display_vbl`, `delivered_vbl_cadence_equals_the_advertised_refresh_rate` |
//! | guest present → publish | one frame is published at the end of a drain tranche, so frames produced inside one tranche collapse to the last | `drain_duty max_tranche_us`, `window_publish` |
//! | publish → window wake | event-driven (`FramePublished` → `request_redraw`), with a backstop timer; no fixed rate | `host_window/present.rs` `about_to_wait` |
//! | Vulkan present | MAILBOX with three images where the surface offers it, FIFO otherwise; MAILBOX can present faster than the monitor scans out | `swapchain_recreated present_mode=` |
//! | the monitor | the panel's refresh bounds what is *displayed* whatever `present_hz` says | `host_window_display refresh_mhz=` |
//!
//! No stage is a fixed 60 Hz timer; the one structural 60 Hz is in the guest.

/// Bucket width of the histogram, microseconds.
const BUCKET_US: u64 = 500;
/// Histogram range; anything at or beyond lands in the overflow bucket, which
/// still contributes its exact worst.
const RANGE_US: u64 = 64_000;
const BUCKETS: usize = (RANGE_US / BUCKET_US) as usize + 1;

/// Below this a gap is always the frame cadence, never idleness.
pub(crate) const ACTIVE_BELOW_US: u64 = 50_000;

/// 1/60 s and the next three thresholds the target is written in.
const OVER_16_7_US: u64 = 16_667;
const OVER_33_3_US: u64 = 33_333;
const OVER_50_US: u64 = 50_000;
const OVER_100_US: u64 = 100_000;

/// What the clock, the drain worker and the presenter said at one present.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PresentStamp {
    /// Monotonic microseconds.
    pub now_us: u64,
    /// Total microseconds the drain worker has ever spent waiting for a doorbell.
    pub drain_idle_total_us: u64,
    /// Total busy refusals (fence, acquire, no area) the presenter has counted.
    pub busy_total: u64,
}

#[derive(Debug)]
pub(crate) struct FrameIntervals {
    last: Option<PresentStamp>,
    hist: [u32; BUCKETS],
    active_n: u64,
    worst_us: u64,
    over_16_7: u64,
    over_33_3: u64,
    over_50: u64,
    over_100: u64,
    idle_gaps: u64,
    idle_max_us: u64,
}

impl Default for FrameIntervals {
    fn default() -> Self {
        Self {
            last: None,
            hist: [0; BUCKETS],
            active_n: 0,
            worst_us: 0,
            over_16_7: 0,
            over_33_3: 0,
            over_50: 0,
            over_100: 0,
            idle_gaps: 0,
            idle_max_us: 0,
        }
    }
}

/// Whether a gap of `gap_us` was the host's.
fn host_attributable(gap_us: u64, idle_in_gap_us: u64, busy_in_gap: u64) -> bool {
    if gap_us < ACTIVE_BELOW_US {
        return true;
    }
    busy_in_gap > 0 || idle_in_gap_us.saturating_mul(2) < gap_us
}

impl FrameIntervals {
    /// One frame reached the queue.
    pub(crate) fn note_present(&mut self, at: PresentStamp) {
        let Some(prev) = self.last.replace(at) else {
            return;
        };
        let gap = at.now_us.saturating_sub(prev.now_us);
        let idle = at
            .drain_idle_total_us
            .saturating_sub(prev.drain_idle_total_us);
        let busy = at.busy_total.saturating_sub(prev.busy_total);
        if !host_attributable(gap, idle, busy) {
            self.idle_gaps += 1;
            self.idle_max_us = self.idle_max_us.max(gap);
            return;
        }
        self.active_n += 1;
        self.worst_us = self.worst_us.max(gap);
        let slot = ((gap / BUCKET_US) as usize).min(BUCKETS - 1);
        self.hist[slot] = self.hist[slot].saturating_add(1);
        self.over_16_7 += u64::from(gap > OVER_16_7_US);
        self.over_33_3 += u64::from(gap > OVER_33_3_US);
        self.over_50 += u64::from(gap > OVER_50_US);
        self.over_100 += u64::from(gap > OVER_100_US);
    }

    /// Upper edge of the bucket holding the `q`-th fraction of active gaps, in
    /// microseconds. The overflow bucket reports the exact worst.
    fn percentile_us(&self, q: f64) -> u64 {
        if self.active_n == 0 {
            return 0;
        }
        let rank = ((self.active_n as f64 * q).ceil() as u64).clamp(1, self.active_n);
        let mut seen = 0u64;
        for (slot, count) in self.hist.iter().enumerate() {
            seen += u64::from(*count);
            if seen >= rank {
                return if slot == BUCKETS - 1 {
                    self.worst_us
                } else {
                    (slot as u64 + 1) * BUCKET_US
                };
            }
        }
        self.worst_us
    }

    /// The window's line, zeroing the tallies but keeping the last present so the
    /// gap that straddles two windows is still measured. `None` when no frame
    /// was presented in it.
    pub(crate) fn take_line(&mut self) -> Option<String> {
        if self.active_n == 0 && self.idle_gaps == 0 {
            return None;
        }
        let ms = |us: u64| us as f64 / 1_000.0;
        let line = format!(
            "host_window_intervals active={} idle_gaps={} idle_max_ms={:.1} \
             p50_ms={:.1} p95_ms={:.1} p99_ms={:.1} worst_ms={:.1} \
             over_16_7ms={} over_33_3ms={} over_50ms={} over_100ms={}",
            self.active_n,
            self.idle_gaps,
            ms(self.idle_max_us),
            ms(self.percentile_us(0.50)),
            ms(self.percentile_us(0.95)),
            ms(self.percentile_us(0.99)),
            ms(self.worst_us),
            self.over_16_7,
            self.over_33_3,
            self.over_50,
            self.over_100,
        );
        let last = self.last;
        *self = Self::default();
        self.last = last;
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(now_us: u64, idle: u64, busy: u64) -> PresentStamp {
        PresentStamp {
            now_us,
            drain_idle_total_us: idle,
            busy_total: busy,
        }
    }

    fn field(line: &str, name: &str) -> f64 {
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} missing from {line}"))
            .parse()
            .unwrap()
    }

    /// Feed a steady cadence, with the drain worker idle for whatever the frame
    /// did not need, and optionally one stall.
    fn run(frames: u64, gap_us: u64, host_busy_us: u64, stall: Option<(u64, u64)>) -> String {
        let mut c = FrameIntervals::default();
        let (mut now, mut idle) = (0u64, 0u64);
        c.note_present(stamp(now, idle, 0));
        for i in 0..frames {
            let g = match stall {
                Some((at, len)) if at == i => len,
                _ => gap_us,
            };
            now += g;
            idle += g.saturating_sub(host_busy_us);
            c.note_present(stamp(now, idle, 0));
        }
        c.take_line().expect("frames were presented")
    }

    /// A clean 60 Hz: the percentiles sit at the cadence and nothing is a hitch
    /// beyond the one-frame threshold's rounding.
    #[test]
    fn a_steady_cadence_reads_as_that_cadence() {
        let line = run(600, 16_667, 5_000, None);
        assert_eq!(field(&line, "active"), 600.0);
        assert_eq!(field(&line, "idle_gaps"), 0.0);
        assert!((field(&line, "p50_ms") - 17.0).abs() < 0.6, "{line}");
        assert!((field(&line, "p99_ms") - 17.0).abs() < 0.6, "{line}");
        assert_eq!(field(&line, "over_16_7ms"), 0.0, "{line}");
        assert_eq!(field(&line, "over_50ms"), 0.0, "{line}");
    }

    /// One 120 ms host stall in a second of 60 Hz: the mean barely moves, and
    /// every tail column names it.
    #[test]
    fn one_host_stall_is_a_hitch_the_mean_would_hide() {
        // The worker was busy the whole stall (a compile inside a tranche).
        let mut c = FrameIntervals::default();
        let (mut now, mut idle) = (0u64, 0u64);
        c.note_present(stamp(now, idle, 0));
        for i in 0..60 {
            let g = if i == 30 { 120_000 } else { 16_667 };
            now += g;
            idle += if i == 30 { 4_000 } else { g - 5_000 };
            c.note_present(stamp(now, idle, 0));
        }
        let line = c.take_line().unwrap();
        assert_eq!(field(&line, "active"), 60.0);
        assert_eq!(field(&line, "idle_gaps"), 0.0);
        assert_eq!(field(&line, "over_100ms"), 1.0, "{line}");
        assert_eq!(field(&line, "over_50ms"), 1.0, "{line}");
        assert_eq!(field(&line, "over_33_3ms"), 1.0, "{line}");
        assert!((field(&line, "worst_ms") - 120.0).abs() < 0.01, "{line}");
        // 1 of 60 is above the 98th percentile: p99 reaches the stall, p95 does not.
        assert!(field(&line, "p99_ms") >= 120.0 - 0.01, "{line}");
        assert!(field(&line, "p95_ms") < 20.0, "{line}");
    }

    /// An idle desktop is not a slow one. A half-second with the worker waiting
    /// for the guest the whole time is an idle gap: reported, but out of the
    /// percentiles and the hitch counts.
    #[test]
    fn a_silent_guest_is_an_idle_gap_not_a_hitch() {
        let mut c = FrameIntervals::default();
        c.note_present(stamp(0, 0, 0));
        // A frame, then 500 ms of the worker waiting, then another frame.
        c.note_present(stamp(16_667, 11_000, 0));
        c.note_present(stamp(516_667, 11_000 + 499_000, 0));
        c.note_present(stamp(533_334, 521_000, 0));
        let line = c.take_line().unwrap();
        assert_eq!(field(&line, "idle_gaps"), 1.0, "{line}");
        assert!((field(&line, "idle_max_ms") - 500.0).abs() < 0.01, "{line}");
        assert_eq!(field(&line, "active"), 2.0, "{line}");
        assert_eq!(field(&line, "over_100ms"), 0.0, "{line}");
        assert!(field(&line, "worst_ms") < 20.0, "{line}");
    }

    /// A refused present inside a long gap means a frame was waiting: the host's
    /// gap, even if the drain worker was idle for all of it.
    #[test]
    fn a_busy_refusal_makes_a_long_gap_the_hosts() {
        assert!(!host_attributable(500_000, 499_000, 0));
        assert!(host_attributable(500_000, 499_000, 1));
        // Below the active threshold nothing is ever idle.
        assert!(host_attributable(40_000, 40_000, 0));
        // Exactly half idle is idle; one microsecond less is the host's.
        assert!(!host_attributable(100_000, 50_000, 0));
        assert!(host_attributable(100_000, 49_999, 0));
    }

    /// The gap that straddles two reporting windows is measured, not dropped.
    #[test]
    fn the_last_present_survives_a_report() {
        let mut c = FrameIntervals::default();
        c.note_present(stamp(0, 0, 0));
        c.note_present(stamp(10_000, 0, 0));
        assert!(c.take_line().is_some());
        assert!(c.take_line().is_none());
        c.note_present(stamp(26_000, 0, 0));
        let line = c.take_line().unwrap();
        assert_eq!(field(&line, "active"), 1.0);
        assert!((field(&line, "worst_ms") - 16.0).abs() < 0.01, "{line}");
    }

    /// 120 Hz is expressible: an 8.33 ms cadence resolves at 0.5 ms and the
    /// 12 ms heartbeat-quantised intervals a 4 ms poll can produce are visible
    /// rather than rounded into the mean.
    #[test]
    fn a_120_hz_cadence_and_its_poll_quantisation_are_distinguishable() {
        let mut c = FrameIntervals::default();
        let (mut now, mut idle) = (0u64, 0u64);
        c.note_present(stamp(now, idle, 0));
        for i in 0..240u64 {
            // 8 ms, 8 ms, 12 ms ... around an 8.33 ms mean.
            let g = if i % 6 == 5 { 12_000 } else { 8_000 };
            now += g;
            idle += g - 2_000;
            c.note_present(stamp(now, idle, 0));
        }
        let line = c.take_line().unwrap();
        assert!(field(&line, "p50_ms") <= 8.5 + 1e-9, "{line}");
        assert!(field(&line, "p99_ms") >= 12.0 - 1e-9, "{line}");
        assert_eq!(field(&line, "over_16_7ms"), 0.0, "{line}");
    }
}
