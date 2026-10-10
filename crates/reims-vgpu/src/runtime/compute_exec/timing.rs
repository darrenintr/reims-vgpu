//! Wall time and output volume of Vulkan dispatches, reported once a second.
//! Observation only: neither timings nor byte totals select a route.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(super) enum Phase {
    Prepare,
    Stage,
    Plan,
    Engine,
    Writeback,
}

impl Phase {
    const fn index(self) -> usize {
        match self {
            Self::Prepare => 0,
            Self::Stage => 1,
            Self::Plan => 2,
            Self::Engine => 3,
            Self::Writeback => 4,
        }
    }
}

pub(super) struct DispatchTiming {
    phase: Phase,
    since: Instant,
    us: [u64; 5],
    completed: bool,
    host_bytes: u64,
    direct_bytes: u64,
}

impl DispatchTiming {
    pub(super) fn new() -> Self {
        Self {
            phase: Phase::Prepare,
            since: Instant::now(),
            us: [0; 5],
            completed: false,
            host_bytes: 0,
            direct_bytes: 0,
        }
    }

    pub(super) fn enter(&mut self, phase: Phase) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.since).as_micros() as u64;
        let cell = &mut self.us[self.phase.index()];
        *cell = cell.saturating_add(elapsed);
        self.since = now;
        self.phase = phase;
    }

    pub(super) fn outputs(&mut self, host_bytes: u64, direct_bytes: u64) {
        self.host_bytes = host_bytes;
        self.direct_bytes = direct_bytes;
    }

    pub(super) fn complete(&mut self) {
        self.completed = true;
    }
}

struct Window {
    start: Instant,
    us: [u64; 5],
    completed: u64,
    refused: u64,
    host_bytes: u64,
    direct_bytes: u64,
}

impl Window {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            us: [0; 5],
            completed: 0,
            refused: 0,
            host_bytes: 0,
            direct_bytes: 0,
        }
    }
}

static WINDOW: Mutex<Option<Window>> = Mutex::new(None);

impl Drop for DispatchTiming {
    fn drop(&mut self) {
        // Charge the last phase even on a refusal or early return.
        self.enter(self.phase);
        let mut guard = WINDOW.lock().unwrap_or_else(|poison| poison.into_inner());
        let window = guard.get_or_insert_with(Window::new);
        for (total, elapsed) in window.us.iter_mut().zip(self.us) {
            *total = total.saturating_add(elapsed);
        }
        if self.completed {
            window.completed = window.completed.saturating_add(1);
        } else {
            window.refused = window.refused.saturating_add(1);
        }
        window.host_bytes = window.host_bytes.saturating_add(self.host_bytes);
        window.direct_bytes = window.direct_bytes.saturating_add(self.direct_bytes);
        let elapsed = window.start.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let line = format!(
            "compute_phase window_ms={} completed={} refused={} prepare_us={} stage_us={} \
             plan_us={} engine_us={} writeback_us={} host_bytes={} direct_bytes={}",
            elapsed.as_millis(),
            window.completed,
            window.refused,
            window.us[0],
            window.us[1],
            window.us[2],
            window.us[3],
            window.us[4],
            window.host_bytes,
            window.direct_bytes,
        );
        *window = Window::new();
        drop(guard);
        crate::observe::off(line);
    }
}
