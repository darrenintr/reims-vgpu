//! What one drain tranche was made of.
//!
//! # The question every other line here cannot answer
//!
//! `drain_duty` reports `max_tranche_us`, and on a driven x86 Ventura boot that
//! figure reached 97 ms and 256 ms while the same window averaged 33-35
//! presents a second. Every other column on that line, and every line emitted
//! under it, is a **sum over the window**. A sum over a second that contains
//! thirty 6 ms tranches and one 256 ms tranche says nothing about what was
//! inside the 256 ms one: a 40 ms `fence_us` could be one long wait in it or
//! forty short ones spread across the others, and those are different defects.
//!
//! A frame-pacing hitch *is* one tranche. So this ledger is reset when a
//! tranche starts, charged by the same hooks that feed the window census, and
//! snapshotted when the tranche ends. Two lines come out of it:
//!
//! - `drain_long`, at once, for every tranche at or above [`LONG_TRANCHE_US`]
//!   (bounded per window by [`LONG_LINES_PER_WINDOW`], with the excess counted
//!   rather than lost). It carries `t=` so a hitch seen on screen can be matched
//!   to the line that explains it.
//! - `drain_worst`, beside `drain_duty`, for the longest tranche of the window
//!   whatever its length — so the tail is described even on a window with no
//!   tranche past the threshold.
//!
//! # How to read a line
//!
//! The first seven costs tile `drain_us`: `setup + ring + decode + regs + proc
//! + tail + boundary + other == drain_us`, where `other` is the residue no hook
//! claimed (the wrapping `Device::drain` work between FIFOs, the main FIFO and
//! iosfc drains). `publish` is outside `drain_us`, as on `drain_duty`, and
//! `drain_us + publish_us == total_us`.
//!
//! Everything else is **inside** `proc` (or, for the deferred-batch submit,
//! inside `tail`) and overlaps by construction — a readback fence is inside a
//! flush, which is inside a draw's packet. They are cross-cuts, not a second
//! tiling, and they are what names the defect:
//!
//! | cost | what it is |
//! |---|---|
//! | `draw` / `compute` / `flush_*` | the `drain_duty` phases, this tranche only |
//! | `rb_*` | `readback_split`'s phases, this tranche only |
//! | `rb_gpu_bar` / `rb_gpu_copy` | GPU timestamps of readbacks *retired* in this tranche |
//! | `ch_*` | `chain_phase`'s columns, this tranche only |
//! | `pipe_create` | `vkCreate{Graphics,Compute}Pipelines`, breadcrumb and cache persist included |
//! | `ring_wait` | the submission ring blocking on an unfinished fence |
//! | `entry_wait` | a synchronous reader waiting its own copy's fence |
//! | `lock_wait` | the drain worker blocked on the engine lock (the window thread holds it) |
//! | `debt_pay` | lazy-writeback payments: a resident's frame landing in guest pages |
//! | `overlay` | guest CPU writes laid over a live resident |
//!
//! `top_op`/`top_us` is the single most expensive packet of the tranche, so a
//! line also says whether the time was one packet or spread over `packets`.
//!
//! # Cost
//!
//! One thread-local flag read per charge outside a tranche, and a relaxed
//! atomic add inside one. The snapshot is ~40 relaxed loads once per tranche.
//! Charges from any thread other than the one running the tranche are ignored:
//! the vCPU and window threads reach the same engine paths, and folding their
//! time into a tranche they did not run would make the line lie about which
//! tranche was long.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// A tranche at or above this is reported on its own line, at once.
///
/// Three frames at 60 Hz: past it a hitch is visible to anyone watching an
/// animation, which is the population this line exists to explain. A shorter
/// tranche still appears as the window's `drain_worst` when it is the longest.
pub const LONG_TRANCHE_US: u64 = 50_000;

/// At most this many `drain_long` lines between two `drain_worst` lines.
///
/// A device that is slow on every tranche would otherwise write a line per
/// tranche; the excess is reported as `long_suppressed` on `drain_worst`, so a
/// silent stretch still says how many it did not print.
pub const LONG_LINES_PER_WINDOW: u64 = 8;

/// One thing a tranche can spend time on. See the module doc for which of these
/// tile `drain_us` and which cut across it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrancheCost {
    Setup,
    Ring,
    Decode,
    Regs,
    Proc,
    Tail,
    Boundary,
    Draw,
    Compute,
    FlushRender,
    FlushGva,
    FlushLinear,
    FlushStorage,
    RbSubmit,
    RbFence,
    RbMap,
    RbWrite,
    RbVouch,
    RbResolve,
    RbGpuBar,
    RbGpuCopy,
    ChPipeline,
    ChAir,
    ChXlate,
    ChBinds,
    ChSampled,
    ChSeed,
    ChAssemble,
    ChEngine,
    ChStore,
    PipeCreate,
    RingWait,
    EntryWait,
    LockWait,
    DebtPay,
    Overlay,
}

impl TrancheCost {
    pub const COUNT: usize = TrancheCost::Overlay as usize + 1;

    pub const ALL: [TrancheCost; Self::COUNT] = [
        TrancheCost::Setup,
        TrancheCost::Ring,
        TrancheCost::Decode,
        TrancheCost::Regs,
        TrancheCost::Proc,
        TrancheCost::Tail,
        TrancheCost::Boundary,
        TrancheCost::Draw,
        TrancheCost::Compute,
        TrancheCost::FlushRender,
        TrancheCost::FlushGva,
        TrancheCost::FlushLinear,
        TrancheCost::FlushStorage,
        TrancheCost::RbSubmit,
        TrancheCost::RbFence,
        TrancheCost::RbMap,
        TrancheCost::RbWrite,
        TrancheCost::RbVouch,
        TrancheCost::RbResolve,
        TrancheCost::RbGpuBar,
        TrancheCost::RbGpuCopy,
        TrancheCost::ChPipeline,
        TrancheCost::ChAir,
        TrancheCost::ChXlate,
        TrancheCost::ChBinds,
        TrancheCost::ChSampled,
        TrancheCost::ChSeed,
        TrancheCost::ChAssemble,
        TrancheCost::ChEngine,
        TrancheCost::ChStore,
        TrancheCost::PipeCreate,
        TrancheCost::RingWait,
        TrancheCost::EntryWait,
        TrancheCost::LockWait,
        TrancheCost::DebtPay,
        TrancheCost::Overlay,
    ];

    /// The costs whose sum, with `other`, is `drain_us`.
    const TILING: [TrancheCost; 7] = [
        TrancheCost::Setup,
        TrancheCost::Ring,
        TrancheCost::Decode,
        TrancheCost::Regs,
        TrancheCost::Proc,
        TrancheCost::Tail,
        TrancheCost::Boundary,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    pub const fn label(self) -> &'static str {
        match self {
            TrancheCost::Setup => "setup",
            TrancheCost::Ring => "ring",
            TrancheCost::Decode => "decode",
            TrancheCost::Regs => "regs",
            TrancheCost::Proc => "proc",
            TrancheCost::Tail => "tail",
            TrancheCost::Boundary => "boundary",
            TrancheCost::Draw => "draw",
            TrancheCost::Compute => "compute",
            TrancheCost::FlushRender => "flush_render",
            TrancheCost::FlushGva => "flush_gva",
            TrancheCost::FlushLinear => "flush_linear",
            TrancheCost::FlushStorage => "flush_storage",
            TrancheCost::RbSubmit => "rb_submit",
            TrancheCost::RbFence => "rb_fence",
            TrancheCost::RbMap => "rb_map",
            TrancheCost::RbWrite => "rb_write",
            TrancheCost::RbVouch => "rb_vouch",
            TrancheCost::RbResolve => "rb_resolve",
            TrancheCost::RbGpuBar => "rb_gpu_bar",
            TrancheCost::RbGpuCopy => "rb_gpu_copy",
            TrancheCost::ChPipeline => "ch_pipeline",
            TrancheCost::ChAir => "ch_air",
            TrancheCost::ChXlate => "ch_xlate",
            TrancheCost::ChBinds => "ch_binds",
            TrancheCost::ChSampled => "ch_sampled",
            TrancheCost::ChSeed => "ch_seed",
            TrancheCost::ChAssemble => "ch_assemble",
            TrancheCost::ChEngine => "ch_engine",
            TrancheCost::ChStore => "ch_store",
            TrancheCost::PipeCreate => "pipe_create",
            TrancheCost::RingWait => "ring_wait",
            TrancheCost::EntryWait => "entry_wait",
            TrancheCost::LockWait => "lock_wait",
            TrancheCost::DebtPay => "debt_pay",
            TrancheCost::Overlay => "overlay",
        }
    }
}

/// One finished tranche, as the ledger saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrancheAnatomy {
    /// When the tranche ended, in [`crate::observe::elapsed_ms`].
    pub at_ms: u64,
    pub drain_us: u64,
    pub publish_us: u64,
    pub ns: [u64; TrancheCost::COUNT],
    pub n: [u64; TrancheCost::COUNT],
    pub packets: u64,
    /// The single most expensive packet's opcode, and its `proc` nanoseconds.
    pub top_op: u16,
    pub top_ns: u64,
}

impl TrancheAnatomy {
    pub fn total_us(&self) -> u64 {
        self.drain_us.saturating_add(self.publish_us)
    }

    pub fn us(&self, cost: TrancheCost) -> u64 {
        self.ns[cost.index()] / 1000
    }

    #[cfg(test)]
    pub fn count(&self, cost: TrancheCost) -> u64 {
        self.n[cost.index()]
    }

    /// `drain_us` that none of the tiling costs claimed.
    pub fn other_us(&self) -> u64 {
        let claimed: u64 = TrancheCost::TILING.iter().map(|&c| self.us(c)).sum();
        self.drain_us.saturating_sub(claimed)
    }

    /// The line body. Zero costs are left off, so a line names only what this
    /// tranche actually did; every printed `_us` carries its count beside it.
    pub fn render(&self, kind: &str) -> String {
        let mut line = format!(
            "{kind} t={} total_us={} drain_us={} publish_us={} other_us={} packets={} \
             top_op={:#04x} top_us={}",
            self.at_ms,
            self.total_us(),
            self.drain_us,
            self.publish_us,
            self.other_us(),
            self.packets,
            self.top_op,
            self.top_ns / 1000,
        );
        for cost in TrancheCost::ALL {
            let i = cost.index();
            if self.ns[i] == 0 && self.n[i] == 0 {
                continue;
            }
            let label = cost.label();
            line.push_str(&format!(
                " {label}_us={} {label}_n={}",
                self.ns[i] / 1000,
                self.n[i]
            ));
        }
        line
    }
}

/// The running tranche's costs, plus the window's worst and the long-line
/// budget. One per process, like every census here; owned by the drain worker.
pub struct TrancheLedger {
    ns: [AtomicU64; TrancheCost::COUNT],
    n: [AtomicU64; TrancheCost::COUNT],
    packets: AtomicU64,
    top_op: AtomicU64,
    top_ns: AtomicU64,
    worst: parking_lot::Mutex<Option<TrancheAnatomy>>,
    long_emitted: AtomicU64,
    long_suppressed: AtomicU64,
    long_total: AtomicU64,
}

impl Default for TrancheLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl TrancheLedger {
    pub const fn new() -> Self {
        Self {
            ns: [const { AtomicU64::new(0) }; TrancheCost::COUNT],
            n: [const { AtomicU64::new(0) }; TrancheCost::COUNT],
            packets: AtomicU64::new(0),
            top_op: AtomicU64::new(0),
            top_ns: AtomicU64::new(0),
            worst: parking_lot::Mutex::new(None),
            long_emitted: AtomicU64::new(0),
            long_suppressed: AtomicU64::new(0),
            long_total: AtomicU64::new(0),
        }
    }

    /// Forget everything charged since the last tranche ended.
    pub fn begin(&self) {
        for i in 0..TrancheCost::COUNT {
            self.ns[i].store(0, Relaxed);
            self.n[i].store(0, Relaxed);
        }
        self.packets.store(0, Relaxed);
        self.top_op.store(0, Relaxed);
        self.top_ns.store(0, Relaxed);
    }

    pub fn charge(&self, cost: TrancheCost, ns: u64) {
        let i = cost.index();
        self.ns[i].fetch_add(ns, Relaxed);
        self.n[i].fetch_add(1, Relaxed);
    }

    /// One dispatched packet: charged to `proc`, counted, and kept when it is
    /// the most expensive of the tranche so far.
    pub fn charge_packet(&self, opcode: u16, ns: u64) {
        self.charge(TrancheCost::Proc, ns);
        self.packets.fetch_add(1, Relaxed);
        if ns > self.top_ns.load(Relaxed) {
            self.top_ns.store(ns, Relaxed);
            self.top_op.store(u64::from(opcode), Relaxed);
        }
    }

    /// Close the tranche: snapshot it, keep it if it is the window's worst, and
    /// return the `drain_long` line when it is long and the budget allows.
    pub fn finish(&self, at_ms: u64, drain_us: u64, publish_us: u64) -> Option<String> {
        let anatomy = TrancheAnatomy {
            at_ms,
            drain_us,
            publish_us,
            ns: std::array::from_fn(|i| self.ns[i].load(Relaxed)),
            n: std::array::from_fn(|i| self.n[i].load(Relaxed)),
            packets: self.packets.load(Relaxed),
            top_op: self.top_op.load(Relaxed) as u16,
            top_ns: self.top_ns.load(Relaxed),
        };
        let long = anatomy.total_us() >= LONG_TRANCHE_US;
        let line = match long {
            true => {
                self.long_total.fetch_add(1, Relaxed);
                if self.long_emitted.fetch_add(1, Relaxed) < LONG_LINES_PER_WINDOW {
                    Some(anatomy.render("drain_long"))
                } else {
                    self.long_suppressed.fetch_add(1, Relaxed);
                    None
                }
            }
            false => None,
        };
        let mut worst = self.worst.lock();
        if worst
            .as_ref()
            .is_none_or(|w| anatomy.total_us() > w.total_us())
        {
            *worst = Some(anatomy);
        }
        line
    }

    /// The window's worst tranche as a `drain_worst` line, and reset the window.
    /// `None` when no tranche finished since the last call.
    pub fn take_worst(&self, win_ms: u64) -> Option<String> {
        let worst = self.worst.lock().take()?;
        self.long_emitted.store(0, Relaxed);
        let long = self.long_total.swap(0, Relaxed);
        let suppressed = self.long_suppressed.swap(0, Relaxed);
        Some(format!(
            "{} win_ms={win_ms} long={long} long_suppressed={suppressed} long_us={LONG_TRANCHE_US}",
            worst.render("drain_worst")
        ))
    }
}

/// The process's ledger.
static TRANCHE: TrancheLedger = TrancheLedger::new();

thread_local! {
    /// Whether this thread is inside a tranche. Only that thread's charges are
    /// the tranche's; see the module doc.
    static IN_TRANCHE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn in_tranche() -> bool {
    IN_TRANCHE.with(std::cell::Cell::get)
}

/// A tranche is starting on this thread.
pub(super) fn tranche_begin() {
    TRANCHE.begin();
    IN_TRANCHE.with(|c| c.set(true));
}

/// The tranche on this thread has ended. Emits `drain_long` when it was long.
pub(super) fn tranche_end(drain_us: u64, publish_us: u64) {
    if !in_tranche() {
        return;
    }
    IN_TRANCHE.with(|c| c.set(false));
    if let Some(line) = TRANCHE.finish(crate::observe::elapsed_ms() as u64, drain_us, publish_us) {
        crate::observe::off(line);
    }
}

/// The window's worst tranche, for the per-second census block.
pub(super) fn take_worst_tranche(win_ms: u64) -> Option<String> {
    TRANCHE.take_worst(win_ms)
}

/// Charge `ns` to `cost` when this thread is running a tranche.
pub fn note_tranche_cost(cost: TrancheCost, ns: u64) {
    if in_tranche() {
        TRANCHE.charge(cost, ns);
    }
}

/// Charge elapsed time since `started` to `cost`.
pub fn note_tranche_since(cost: TrancheCost, started: std::time::Instant) {
    if in_tranche() {
        TRANCHE.charge(cost, started.elapsed().as_nanos() as u64);
    }
}

pub(super) fn note_tranche_packet(opcode: u16, ns: u64) {
    if in_tranche() {
        TRANCHE.charge_packet(opcode, ns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger_with(costs: &[(TrancheCost, u64)]) -> TrancheLedger {
        let l = TrancheLedger::new();
        l.begin();
        for &(c, ns) in costs {
            l.charge(c, ns);
        }
        l
    }

    #[test]
    fn the_tiling_and_other_sum_to_drain_us() {
        let l = ledger_with(&[
            (TrancheCost::Proc, 40_000_000),
            (TrancheCost::Tail, 3_000_000),
            (TrancheCost::Ring, 1_000_000),
            // A cross-cut inside `proc` must not be subtracted a second time.
            (TrancheCost::RbFence, 30_000_000),
        ]);
        l.finish(1, 50_000, 2_000);
        let worst = l.worst.lock().clone().expect("one tranche finished");
        assert_eq!(worst.other_us(), 50_000 - 40_000 - 3_000 - 1_000);
        assert_eq!(worst.total_us(), 52_000);
        assert_eq!(worst.us(TrancheCost::RbFence), 30_000);
    }

    #[test]
    fn begin_forgets_the_previous_tranche() {
        let l = ledger_with(&[(TrancheCost::DebtPay, 9_000_000)]);
        l.charge_packet(0x37, 9_000_000);
        l.begin();
        l.finish(1, 10, 0);
        let worst = l.worst.lock().clone().expect("finished");
        assert_eq!(worst.us(TrancheCost::DebtPay), 0);
        assert_eq!(worst.packets, 0);
        assert_eq!(worst.top_ns, 0);
    }

    #[test]
    fn the_top_packet_is_the_most_expensive_one() {
        let l = TrancheLedger::new();
        l.begin();
        l.charge_packet(0x10, 5_000);
        l.charge_packet(0x37, 90_000);
        l.charge_packet(0x12, 7_000);
        l.finish(1, 100, 0);
        let worst = l.worst.lock().clone().expect("finished");
        assert_eq!(
            (worst.top_op, worst.top_ns, worst.packets),
            (0x37, 90_000, 3)
        );
        assert_eq!(worst.count(TrancheCost::Proc), 3);
        assert_eq!(worst.ns[TrancheCost::Proc.index()], 102_000);
    }

    #[test]
    fn the_window_keeps_its_longest_tranche_and_resets_on_take() {
        let l = TrancheLedger::new();
        for (drain, pay_ns) in [(8_000, 1_000), (97_000, 80_000_000), (12_000, 2_000)] {
            l.begin();
            l.charge(TrancheCost::DebtPay, pay_ns);
            let _ = l.finish(1, drain, 0);
        }
        let line = l.take_worst(1000).expect("three tranches finished");
        assert!(line.starts_with("drain_worst "), "{line}");
        assert!(line.contains(" total_us=97000 "), "{line}");
        assert!(line.contains(" debt_pay_us=80000 debt_pay_n=1"), "{line}");
        assert!(line.contains(" long=1 long_suppressed=0 "), "{line}");
        assert!(l.take_worst(1000).is_none(), "the window was reset");
    }

    #[test]
    fn long_tranches_print_at_once_up_to_the_budget() {
        let l = TrancheLedger::new();
        let mut printed = 0;
        for _ in 0..LONG_LINES_PER_WINDOW + 3 {
            l.begin();
            if let Some(line) = l.finish(5, LONG_TRANCHE_US, 0) {
                assert!(line.starts_with("drain_long t=5 "), "{line}");
                printed += 1;
            }
        }
        assert!(
            l.finish(5, LONG_TRANCHE_US - 1, 0).is_none(),
            "below the threshold"
        );
        assert_eq!(printed, LONG_LINES_PER_WINDOW);
        let line = l.take_worst(1000).expect("finished");
        assert!(
            line.contains(&format!(
                " long={} long_suppressed=3 ",
                LONG_LINES_PER_WINDOW + 3
            )),
            "{line}"
        );
        l.begin();
        assert!(
            l.finish(6, LONG_TRANCHE_US, 0).is_some(),
            "the budget is per window"
        );
    }

    #[test]
    fn a_line_names_only_what_the_tranche_did() {
        let l = ledger_with(&[(TrancheCost::PipeCreate, 120_000_000)]);
        l.finish(1, 130_000, 0);
        let line = l
            .worst
            .lock()
            .clone()
            .expect("finished")
            .render("drain_long");
        assert!(
            line.contains(" pipe_create_us=120000 pipe_create_n=1"),
            "{line}"
        );
        assert!(!line.contains("rb_fence"), "{line}");
    }

    #[test]
    fn charges_off_the_tranche_thread_are_ignored() {
        // The global ledger, through the public hooks: a thread that never
        // began a tranche cannot charge one.
        std::thread::spawn(|| {
            assert!(!in_tranche());
            note_tranche_cost(TrancheCost::LockWait, 1);
        })
        .join()
        .expect("thread");
    }

    #[test]
    fn labels_are_unique_and_all_is_complete() {
        let mut seen = std::collections::BTreeSet::new();
        for (i, c) in TrancheCost::ALL.iter().enumerate() {
            assert_eq!(c.index(), i);
            assert!(seen.insert(c.label()), "duplicate label {}", c.label());
        }
    }
}
