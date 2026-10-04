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
//! snapshotted when the tranche ends. Four lines come out of it:
//!
//! - `drain_long`, at once, for every tranche at or above [`LONG_TRANCHE_US`]
//!   (bounded per window by [`LONG_LINES_PER_WINDOW`], with the excess counted
//!   rather than lost). It carries `t=` so a hitch seen on screen can be matched
//!   to the line that explains it.
//! - `drain_worst`, beside `drain_duty`, for the longest tranche of the window
//!   whatever its length — so the tail is described even on a window with no
//!   tranche past the threshold.
//! - `present_long` / `present_worst`, the same pair for one present packet
//!   (`DisplaySwap`, x86 `PRESENT_X86` op 6/7): what `present_named_mapping`
//!   spent its time on. See [`PresentPhase`].
//!
//! # The tiling is exclusive, and why it has to be
//!
//! The first version of this ledger timed each packet's dispatch as one
//! inclusive `proc` span. A present packet drains *other* channels before it
//! retains its surface (`drain_other_child_fifos`), and the packets those drains
//! run were timed as `proc` again — inside the present's own `proc`. A live
//! RX 7600 line read `drain_us=61285 proc_us=62884`: a component of a tiling
//! larger than the whole. And everything the hooks did not reach (the root
//! FIFO, the model settle, iosfc, the retired-view flush) fell into one `other`
//! that measured 99.9 % of a 1.04 s tranche.
//!
//! So the tiling is now a stack of **exclusive spans**. Every span charges its
//! cost only for the time none of its children claimed; a child's whole
//! inclusive time is credited to its nearest tiling ancestor; leaf charges
//! (`ring`, `decode`, `setup`, `regs`) are credited the same way. A nested
//! drain inside a present is therefore counted once: its packets' `proc`, its
//! `setup`/`ring`/`regs`, its `settle`, each under its own name, and the
//! present's `proc` is only what the present itself did.
//!
//! The tiling is `TrancheCost::TILING`:
//!
//! `setup + ring + decode + regs + proc + tail + boundary + root_proc +
//! xlate_retry + main_fifo + sweep + resweep + child_fifo + admit + settle +
//! settle_stamps + settle_pump + complete + iosfc + display_online +
//! retired_views + retire_linear + other == drain_us`
//!
//! `other` is now only what lies between the worker's own clock read and the
//! first span — tens of nanoseconds. It is computed, not clamped: if the claimed
//! costs ever exceed `drain_us` the line says `overrun_us=` instead of printing
//! a zero `other`, because a tiling that overruns its whole is an instrument
//! defect and must not read like a clean line.
//!
//! `publish` is outside `drain_us`, as on `drain_duty`, and
//! `drain_us + publish_us == total_us`.
//!
//! # The cross-cuts
//!
//! Everything else overlaps by construction — a readback fence is inside a
//! flush, which is inside a draw's packet. They are not a second tiling, and
//! they are what names the defect:
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
//! | `alloc` | `vkAllocateMemory` — new residents, staging and pool growth |
//! | `nested_drain` | inclusive time of drains entered from inside a packet (`_n` = packets that did) |
//! | `pump_preflight` | `settle_pump` re-running a parked position's translation preflight |
//! | `settle_walk` | `_n` only: parked positions walked by `settle_stamps` and `settle_pump` |
//! | `host_map` / `host_unmap` | the QEMU shim's `map_pages` / `unmap_pages` callbacks |
//! | `host_track` / `host_untrack` | the QEMU shim's guest-write (dirty-log) token callbacks |
//!
//! `top_op`/`top_us` is the single most expensive packet of the tranche by
//! *inclusive* time — what the packet cost the guest, nested drains and all —
//! and `top_self_us` is the same packet's exclusive `proc`. A present whose
//! `top_us` is far above its `top_self_us` spent its time draining other
//! channels, and the `present_long` line says in which rescue.
//!
//! # Cost
//!
//! One thread-local flag read per charge outside a tranche. Inside one, a span
//! is two clock reads and a push/pop on a fixed array — no allocation — and a
//! leaf charge is two adds. Charges from any thread other than the one running
//! the tranche are ignored: the ledger *is* thread-local, so the vCPU and
//! window threads, which reach the same engine paths, cannot fold their time
//! into a tranche they did not run.

use std::cell::RefCell;

/// A tranche at or above this is reported on its own line, at once.
///
/// Three frames at 60 Hz: past it a hitch is visible to anyone watching an
/// animation, which is the population this line exists to explain. A shorter
/// tranche still appears as the window's `drain_worst` when it is the longest.
pub const LONG_TRANCHE_US: u64 = 50_000;

/// A present packet at or above this is reported as `present_long`, at once.
///
/// One frame at 60 Hz. A present is a single packet and everything behind it
/// on the display waits for it, so a present that alone costs a frame is the
/// unit of a visible stall.
pub const LONG_PRESENT_US: u64 = 16_000;

/// At most this many `drain_long` (and, separately, `present_long`) lines
/// between two `drain_worst` lines.
///
/// A device that is slow on every tranche would otherwise write a line per
/// tranche; the excess is reported as `long_suppressed` on `drain_worst`, so a
/// silent stretch still says how many it did not print.
pub const LONG_LINES_PER_WINDOW: u64 = 8;

/// How deep the span stack goes before a span is refused (and counted as
/// `depth_overflow`). A present's rescue drain runs a packet that can itself
/// rescue, so the realistic depth is ~12; the bound is only so the stack can be
/// a fixed array.
const MAX_DEPTH: usize = 48;

/// How many present packets can be open at once (a present whose rescue drain
/// runs another present).
const MAX_PRESENT_DEPTH: usize = 4;

/// One thing a tranche can spend time on. See the module doc for which of these
/// tile `drain_us` and which cut across it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrancheCost {
    // ---- the exclusive tiling of `drain_us` ----
    /// `drain_child_fifo`'s prologue: register reads and the ring page list.
    Setup,
    /// Ring snapshot reads, both FIFOs.
    Ring,
    /// Packet decode, both FIFOs.
    Decode,
    /// Child register accesses around a packet.
    Regs,
    /// A child packet's dispatch, exclusive of any drain it nests.
    Proc,
    /// The deferred-batch submit after `Device::drain`.
    Tail,
    /// The present boundary publish after `Device::drain`.
    Boundary,
    /// A root (main FIFO) packet's dispatch.
    RootProc,
    /// `drain_pending`'s translation release and the deferred-EXEC retry.
    XlateRetry,
    /// `drain_main_fifo`'s own loop, exclusive of its packets and settles.
    MainFifo,
    /// `drain_pending`'s own control flow: the child sweep and its refills.
    Sweep,
    /// `drain_other_child_fifos`' own control flow (the rescue sweep).
    Resweep,
    /// `drain_child_fifo`'s own loop, exclusive of everything named here.
    ChildFifo,
    /// Admission: stamp-wait census, arrival work, parking, preflight.
    Admit,
    /// `settle_model_work`'s own walk: take/mark ready, the ready list, the
    /// waiting walk that re-arms domains.
    Settle,
    /// `observe_awaited_stamps`: reading the slots parked work waits on.
    SettleStamps,
    /// `pump_translations`: re-planning translation-parked positions.
    SettlePump,
    /// Completing a run position and publishing its stamp word.
    Complete,
    /// `drain_iosfc`.
    Iosfc,
    /// `try_display_online`.
    DisplayOnline,
    /// `mapper::flush_retired_views`.
    RetiredViews,
    /// `render_writeback::retire_linear_residents`.
    RetireLinear,
    // ---- cross-cuts ----
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
    Alloc,
    NestedDrain,
    PumpPreflight,
    SettleWalk,
    HostMap,
    HostUnmap,
    HostTrack,
    HostUntrack,
}

impl TrancheCost {
    pub const COUNT: usize = TrancheCost::HostUntrack as usize + 1;

    /// How many leading variants tile `drain_us`.
    const TILE_COUNT: usize = TrancheCost::RetireLinear as usize + 1;

    pub const ALL: [TrancheCost; Self::COUNT] = [
        TrancheCost::Setup,
        TrancheCost::Ring,
        TrancheCost::Decode,
        TrancheCost::Regs,
        TrancheCost::Proc,
        TrancheCost::Tail,
        TrancheCost::Boundary,
        TrancheCost::RootProc,
        TrancheCost::XlateRetry,
        TrancheCost::MainFifo,
        TrancheCost::Sweep,
        TrancheCost::Resweep,
        TrancheCost::ChildFifo,
        TrancheCost::Admit,
        TrancheCost::Settle,
        TrancheCost::SettleStamps,
        TrancheCost::SettlePump,
        TrancheCost::Complete,
        TrancheCost::Iosfc,
        TrancheCost::DisplayOnline,
        TrancheCost::RetiredViews,
        TrancheCost::RetireLinear,
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
        TrancheCost::Alloc,
        TrancheCost::NestedDrain,
        TrancheCost::PumpPreflight,
        TrancheCost::SettleWalk,
        TrancheCost::HostMap,
        TrancheCost::HostUnmap,
        TrancheCost::HostTrack,
        TrancheCost::HostUntrack,
    ];

    /// The costs whose sum, with `other`, is `drain_us`. Each is exclusive.
    pub const TILING: [TrancheCost; Self::TILE_COUNT] = {
        let mut out = [TrancheCost::Setup; Self::TILE_COUNT];
        let mut i = 0;
        while i < Self::TILE_COUNT {
            out[i] = Self::ALL[i];
            i += 1;
        }
        out
    };

    const fn index(self) -> usize {
        self as usize
    }

    /// Whether this cost is part of the exclusive tiling of `drain_us`.
    pub const fn is_tiling(self) -> bool {
        self.index() < Self::TILE_COUNT
    }

    /// A drain of one FIFO: what "nested drain" means when it is entered from
    /// inside a packet.
    const fn is_drain(self) -> bool {
        matches!(self, TrancheCost::ChildFifo | TrancheCost::MainFifo)
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
            TrancheCost::RootProc => "root_proc",
            TrancheCost::XlateRetry => "xlate_retry",
            TrancheCost::MainFifo => "main_fifo",
            TrancheCost::Sweep => "sweep",
            TrancheCost::Resweep => "resweep",
            TrancheCost::ChildFifo => "child_fifo",
            TrancheCost::Admit => "admit",
            TrancheCost::Settle => "settle",
            TrancheCost::SettleStamps => "settle_stamps",
            TrancheCost::SettlePump => "settle_pump",
            TrancheCost::Complete => "complete",
            TrancheCost::Iosfc => "iosfc",
            TrancheCost::DisplayOnline => "display_online",
            TrancheCost::RetiredViews => "retired_views",
            TrancheCost::RetireLinear => "retire_linear",
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
            TrancheCost::Alloc => "alloc",
            TrancheCost::NestedDrain => "nested_drain",
            TrancheCost::PumpPreflight => "pump_preflight",
            TrancheCost::SettleWalk => "settle_walk",
            TrancheCost::HostMap => "host_map",
            TrancheCost::HostUnmap => "host_unmap",
            TrancheCost::HostTrack => "host_track",
            TrancheCost::HostUntrack => "host_untrack",
        }
    }
}

/// One step of `present_named_mapping`, timed **inclusive** of anything it
/// calls — including drains — so "which step owned the present" has one
/// answer per step. What part of a step was nested drain work is reported
/// beside it as `<step>_nested_us`, and the steps are sequential, so
/// `self_us = total_us - sum(steps)` is the present's own residue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentPhase {
    /// The first `drain_other_child_fifos` Dekker rescue.
    RescueChildFirst,
    /// The second, back-to-back, `drain_other_child_fifos`.
    RescueChildSecond,
    /// `drain_main_fifo`, when the root ring holds unread work.
    RescueMain,
    /// The `drain_other_child_fifos` after the main-ring rescue.
    RescueChildAfterMain,
    /// `writeback_debt::pay_cpu_shared_mapping`.
    PayCpuShared,
    /// `objects::ensure_surface_for_present`.
    EnsureSurface,
    /// `mapper::resolve_mapping_backing` (MappingInternal surfaces only).
    ResolveBacking,
    /// `Backend::present_resident_carries` (unbacked presents only).
    ResidentCarries,
    /// `scanout::note_present_field_witness`.
    FieldWitness,
    /// `scanout::capture_present_frame`.
    Capture,
    /// `bgra_rgb_stats` and the content verdict over captured CPU pixels.
    ContentStats,
    /// `enqueue_present_scanout`.
    EnqueueScanout,
    /// `signal_display_present_complete`.
    SignalComplete,
}

impl PresentPhase {
    pub const COUNT: usize = PresentPhase::SignalComplete as usize + 1;

    pub const ALL: [PresentPhase; Self::COUNT] = [
        PresentPhase::RescueChildFirst,
        PresentPhase::RescueChildSecond,
        PresentPhase::RescueMain,
        PresentPhase::RescueChildAfterMain,
        PresentPhase::PayCpuShared,
        PresentPhase::EnsureSurface,
        PresentPhase::ResolveBacking,
        PresentPhase::ResidentCarries,
        PresentPhase::FieldWitness,
        PresentPhase::Capture,
        PresentPhase::ContentStats,
        PresentPhase::EnqueueScanout,
        PresentPhase::SignalComplete,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    pub const fn label(self) -> &'static str {
        match self {
            PresentPhase::RescueChildFirst => "rescue_child_first",
            PresentPhase::RescueChildSecond => "rescue_child_second",
            PresentPhase::RescueMain => "rescue_main",
            PresentPhase::RescueChildAfterMain => "rescue_child_after_main",
            PresentPhase::PayCpuShared => "pay_cpu_shared",
            PresentPhase::EnsureSurface => "ensure_surface",
            PresentPhase::ResolveBacking => "resolve_backing",
            PresentPhase::ResidentCarries => "resident_carries",
            PresentPhase::FieldWitness => "field_witness",
            PresentPhase::Capture => "capture",
            PresentPhase::ContentStats => "content_stats",
            PresentPhase::EnqueueScanout => "enqueue_scanout",
            PresentPhase::SignalComplete => "signal_complete",
        }
    }
}

/// One present packet, as the ledger saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentAnatomy {
    /// When the present finished, in [`crate::observe::elapsed_ms`].
    pub at_ms: u64,
    pub channel: u32,
    pub mapping: u32,
    /// `present_named_mapping`, entry to return.
    pub total_ns: u64,
    /// The part of `total_ns` spent in drains it entered (any step).
    pub nested_ns: u64,
    pub ns: [u64; PresentPhase::COUNT],
    pub n: [u64; PresentPhase::COUNT],
    pub nested: [u64; PresentPhase::COUNT],
}

impl PresentAnatomy {
    const EMPTY: Self = Self {
        at_ms: 0,
        channel: 0,
        mapping: 0,
        total_ns: 0,
        nested_ns: 0,
        ns: [0; PresentPhase::COUNT],
        n: [0; PresentPhase::COUNT],
        nested: [0; PresentPhase::COUNT],
    };

    pub fn total_us(&self) -> u64 {
        self.total_ns / 1000
    }

    #[cfg(test)]
    pub fn ns_of(&self, phase: PresentPhase) -> u64 {
        self.ns[phase.index()]
    }

    #[cfg(test)]
    pub fn nested_of(&self, phase: PresentPhase) -> u64 {
        self.nested[phase.index()]
    }

    /// `total - sum(steps)`: `Ok` with the present's own residue, or `Err`
    /// with how far the steps overran the whole (an instrument defect, named).
    pub fn residue_ns(&self) -> Result<u64, u64> {
        let steps: u64 = self.ns.iter().sum();
        match self.total_ns.checked_sub(steps) {
            Some(rest) => Ok(rest),
            None => Err(steps - self.total_ns),
        }
    }

    pub fn render(&self, kind: &str) -> String {
        let (self_ns, overrun_ns) = match self.residue_ns() {
            Ok(rest) => (rest, 0),
            Err(over) => (0, over),
        };
        let mut line = format!(
            "{kind} t={} ch={} mid={} total_us={} self_us={} nested_us={}",
            self.at_ms,
            self.channel,
            self.mapping,
            self.total_us(),
            self_ns / 1000,
            self.nested_ns / 1000,
        );
        if overrun_ns != 0 {
            line.push_str(&format!(" overrun_us={}", overrun_ns / 1000));
        }
        for phase in PresentPhase::ALL {
            let i = phase.index();
            if self.ns[i] == 0 && self.n[i] == 0 {
                continue;
            }
            let label = phase.label();
            line.push_str(&format!(
                " {label}_us={} {label}_n={}",
                self.ns[i] / 1000,
                self.n[i]
            ));
            if self.nested[i] != 0 {
                line.push_str(&format!(" {label}_nested_us={}", self.nested[i] / 1000));
            }
        }
        line
    }
}

/// One finished tranche, as the ledger saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrancheAnatomy {
    /// When the tranche ended, in [`crate::observe::elapsed_ms`].
    pub at_ms: u64,
    pub drain_ns: u64,
    pub publish_us: u64,
    pub ns: [u64; TrancheCost::COUNT],
    pub n: [u64; TrancheCost::COUNT],
    /// Child packets dispatched (nested ones included, each once).
    pub packets: u64,
    /// The single most expensive packet's opcode, its inclusive nanoseconds,
    /// and its exclusive (`proc`) nanoseconds.
    pub top_op: u16,
    pub top_ns: u64,
    pub top_self_ns: u64,
    /// Children that measured longer than their parent span: clock skew. Never
    /// expected; reported so the instrument cannot fail quietly.
    pub skew_ns: u64,
    pub skew_n: u64,
    /// Spans still open when the tranche ended, and spans refused for depth.
    pub unclosed: u64,
    pub depth_overflow: u64,
    /// The tranche's most expensive present packet.
    pub worst_present: Option<PresentAnatomy>,
}

impl TrancheAnatomy {
    pub fn drain_us(&self) -> u64 {
        self.drain_ns / 1000
    }

    pub fn total_us(&self) -> u64 {
        self.drain_us().saturating_add(self.publish_us)
    }

    #[cfg(test)]
    pub fn us(&self, cost: TrancheCost) -> u64 {
        self.ns[cost.index()] / 1000
    }

    pub fn ns_of(&self, cost: TrancheCost) -> u64 {
        self.ns[cost.index()]
    }

    #[cfg(test)]
    pub fn count(&self, cost: TrancheCost) -> u64 {
        self.n[cost.index()]
    }

    /// Nanoseconds the exclusive tiling claimed.
    pub fn claimed_ns(&self) -> u64 {
        TrancheCost::TILING.iter().map(|&c| self.ns_of(c)).sum()
    }

    /// `drain_ns` that no tiling cost claimed: `Ok(other)`, or `Err(overrun)`
    /// when the tiling claimed more than the whole — which the exclusive spans
    /// make impossible, and which is therefore reported rather than clamped.
    pub fn residue_ns(&self) -> Result<u64, u64> {
        let claimed = self.claimed_ns();
        match self.drain_ns.checked_sub(claimed) {
            Some(rest) => Ok(rest),
            None => Err(claimed - self.drain_ns),
        }
    }

    /// `drain_us` that none of the tiling costs claimed. Zero when they
    /// overran; [`Self::residue_ns`] says by how much.
    pub fn other_us(&self) -> u64 {
        self.residue_ns().unwrap_or(0) / 1000
    }

    /// The line body. Zero costs are left off, so a line names only what this
    /// tranche actually did; every printed `_us` carries its count beside it.
    pub fn render(&self, kind: &str) -> String {
        let mut line = format!(
            "{kind} t={} total_us={} drain_us={} publish_us={} other_us={}",
            self.at_ms,
            self.total_us(),
            self.drain_us(),
            self.publish_us,
            self.other_us(),
        );
        if let Err(over) = self.residue_ns() {
            line.push_str(&format!(" overrun_us={}", over / 1000));
        }
        line.push_str(&format!(
            " packets={} top_op={:#04x} top_us={} top_self_us={}",
            self.packets,
            self.top_op,
            self.top_ns / 1000,
            self.top_self_ns / 1000,
        ));
        if self.skew_n != 0 {
            line.push_str(&format!(
                " skew_us={} skew_n={}",
                self.skew_ns / 1000,
                self.skew_n
            ));
        }
        if self.unclosed != 0 {
            line.push_str(&format!(" unclosed={}", self.unclosed));
        }
        if self.depth_overflow != 0 {
            line.push_str(&format!(" depth_overflow={}", self.depth_overflow));
        }
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
        if let Some(present) = &self.worst_present {
            line.push_str(&format!(
                " present_us={} present_self_us={} present_nested_us={} present_mid={}",
                present.total_us(),
                present.residue_ns().unwrap_or(0) / 1000,
                present.nested_ns / 1000,
                present.mapping,
            ));
        }
        line
    }
}

/// What an open span is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameKind {
    /// A tiling cost, charged exclusive.
    Tile(TrancheCost),
    /// One child packet; charged exclusive to `proc`.
    Packet(u16),
    /// One present step: inclusive, transparent to the tiling.
    Phase(PresentPhase),
    /// One whole present packet: inclusive, transparent to the tiling.
    Present,
}

impl FrameKind {
    /// Whether this span takes part in the exclusive tiling.
    const fn tiles(self) -> bool {
        matches!(self, FrameKind::Tile(_) | FrameKind::Packet(_))
    }

    const fn is_drain(self) -> bool {
        match self {
            FrameKind::Tile(cost) => cost.is_drain(),
            _ => false,
        }
    }

    /// Whether a drain nested under this span is reported against it.
    const fn collects_nested(self) -> bool {
        matches!(
            self,
            FrameKind::Packet(_) | FrameKind::Phase(_) | FrameKind::Present
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    kind: FrameKind,
    serial: u32,
    start_ns: u64,
    /// Inclusive time of tiling children (spans and leaves).
    child_ns: u64,
    /// Inclusive time of drains nested under this span, outermost only.
    nested_ns: u64,
}

impl Frame {
    const EMPTY: Self = Self {
        kind: FrameKind::Present,
        serial: 0,
        start_ns: 0,
        child_ns: 0,
        nested_ns: 0,
    };
}

/// A span's identity on the stack: its slot and the serial it was pushed with,
/// so a stale guard cannot close whichever span reused its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanSlot {
    index: usize,
    serial: u32,
}

/// What closing a span produced that the ledger itself does not keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed {
    /// For a packet span: its exclusive nanoseconds.
    pub packet_self_ns: Option<u64>,
    /// For a present span: the finished present.
    pub present: Option<PresentAnatomy>,
}

/// The running tranche: costs, the span stack, and the present records.
///
/// Pure: every method takes the time it is told, so the accounting is tested
/// on synthetic clocks. The thread-local instance below feeds it real ones.
pub struct TrancheLedger {
    active: bool,
    ns: [u64; TrancheCost::COUNT],
    n: [u64; TrancheCost::COUNT],
    packets: u64,
    top_op: u16,
    top_ns: u64,
    top_self_ns: u64,
    skew_ns: u64,
    skew_n: u64,
    overflow: u64,
    serial: u32,
    stack: [Frame; MAX_DEPTH],
    depth: usize,
    presents: [PresentAnatomy; MAX_PRESENT_DEPTH],
    present_depth: usize,
    worst_present: Option<PresentAnatomy>,
}

impl Default for TrancheLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl TrancheLedger {
    pub const fn new() -> Self {
        Self {
            active: false,
            ns: [0; TrancheCost::COUNT],
            n: [0; TrancheCost::COUNT],
            packets: 0,
            top_op: 0,
            top_ns: 0,
            top_self_ns: 0,
            skew_ns: 0,
            skew_n: 0,
            overflow: 0,
            serial: 0,
            stack: [Frame::EMPTY; MAX_DEPTH],
            depth: 0,
            presents: [PresentAnatomy::EMPTY; MAX_PRESENT_DEPTH],
            present_depth: 0,
            worst_present: None,
        }
    }

    /// Forget everything charged since the last tranche ended, and start one.
    pub fn begin(&mut self) {
        *self = Self {
            active: true,
            serial: self.serial,
            ..Self::new()
        };
    }

    fn add(&mut self, cost: TrancheCost, ns: u64, n: u64) {
        let i = cost.index();
        self.ns[i] = self.ns[i].saturating_add(ns);
        self.n[i] = self.n[i].saturating_add(n);
    }

    /// Credit `ns` of tiling work to the nearest open tiling span, so that span
    /// does not also charge it as its own.
    fn credit_parent(&mut self, ns: u64) {
        for frame in self.stack[..self.depth].iter_mut().rev() {
            if frame.kind.tiles() {
                frame.child_ns = frame.child_ns.saturating_add(ns);
                return;
            }
        }
    }

    /// A drain closed: report its inclusive time to every enclosing span that
    /// collects nested drains, up to (not past) the next enclosing drain — that
    /// drain's own close reports it, so nothing is reported twice.
    fn credit_nested(&mut self, ns: u64) {
        for frame in self.stack[..self.depth].iter_mut().rev() {
            if frame.kind.is_drain() {
                return;
            }
            if frame.kind.collects_nested() {
                frame.nested_ns = frame.nested_ns.saturating_add(ns);
            }
        }
    }

    /// A leaf charge. Tiling leaves are credited to their parent span.
    pub fn charge(&mut self, cost: TrancheCost, ns: u64) {
        self.charge_n(cost, ns, 1);
    }

    /// A charge that counts `n` events at once.
    pub fn charge_n(&mut self, cost: TrancheCost, ns: u64, n: u64) {
        if !self.active {
            return;
        }
        self.add(cost, ns, n);
        if cost.is_tiling() {
            self.credit_parent(ns);
        }
    }

    fn push(&mut self, kind: FrameKind, now_ns: u64) -> Option<SpanSlot> {
        if !self.active {
            return None;
        }
        if self.depth == MAX_DEPTH {
            self.overflow += 1;
            return None;
        }
        self.serial = self.serial.wrapping_add(1);
        let index = self.depth;
        self.stack[index] = Frame {
            kind,
            serial: self.serial,
            start_ns: now_ns,
            child_ns: 0,
            nested_ns: 0,
        };
        self.depth += 1;
        Some(SpanSlot {
            index,
            serial: self.serial,
        })
    }

    /// Open a span charged exclusive to `cost`, a tiling cost.
    pub fn open(&mut self, cost: TrancheCost, now_ns: u64) -> Option<SpanSlot> {
        debug_assert!(cost.is_tiling(), "{} is a cross-cut", cost.label());
        self.push(FrameKind::Tile(cost), now_ns)
    }

    /// Open one child packet's dispatch.
    pub fn open_packet(&mut self, opcode: u16, now_ns: u64) -> Option<SpanSlot> {
        self.push(FrameKind::Packet(opcode), now_ns)
    }

    /// Open one present step. Charged to the innermost open present.
    pub fn open_phase(&mut self, phase: PresentPhase, now_ns: u64) -> Option<SpanSlot> {
        self.push(FrameKind::Phase(phase), now_ns)
    }

    /// Open one present packet's record.
    pub fn open_present(&mut self, channel: u32, mapping: u32, now_ns: u64) -> Option<SpanSlot> {
        if !self.active {
            return None;
        }
        if self.present_depth == MAX_PRESENT_DEPTH {
            self.overflow += 1;
            return None;
        }
        let slot = self.push(FrameKind::Present, now_ns)?;
        self.presents[self.present_depth] = PresentAnatomy {
            channel,
            mapping,
            ..PresentAnatomy::EMPTY
        };
        self.present_depth += 1;
        Some(slot)
    }

    /// Close the span `slot` names. Spans opened after it and still open are
    /// closed first, at the same instant; a slot already closed is a no-op.
    pub fn close(&mut self, slot: SpanSlot, now_ns: u64) -> Option<Closed> {
        if !self.active || slot.index >= self.depth || self.stack[slot.index].serial != slot.serial
        {
            return None;
        }
        let mut closed = None;
        while self.depth > slot.index {
            closed = Some(self.pop(now_ns));
        }
        closed
    }

    fn pop(&mut self, now_ns: u64) -> Closed {
        self.depth -= 1;
        let frame = self.stack[self.depth];
        let incl = now_ns.saturating_sub(frame.start_ns);
        let excl = match incl.checked_sub(frame.child_ns) {
            Some(excl) => excl,
            None => {
                self.skew_ns = self.skew_ns.saturating_add(frame.child_ns - incl);
                self.skew_n += 1;
                0
            }
        };
        let mut closed = Closed {
            packet_self_ns: None,
            present: None,
        };
        match frame.kind {
            FrameKind::Tile(cost) => {
                self.add(cost, excl, 1);
                self.credit_parent(incl);
                if cost.is_drain() {
                    self.credit_nested(incl);
                }
            }
            FrameKind::Packet(opcode) => {
                self.add(TrancheCost::Proc, excl, 1);
                self.credit_parent(incl);
                self.packets += 1;
                if incl > self.top_ns {
                    self.top_ns = incl;
                    self.top_self_ns = excl;
                    self.top_op = opcode;
                }
                if frame.nested_ns != 0 {
                    self.add(TrancheCost::NestedDrain, frame.nested_ns, 1);
                }
                closed.packet_self_ns = Some(excl);
            }
            FrameKind::Phase(phase) => {
                if self.present_depth != 0 {
                    let record = &mut self.presents[self.present_depth - 1];
                    let i = phase.index();
                    record.ns[i] = record.ns[i].saturating_add(incl);
                    record.n[i] += 1;
                    record.nested[i] = record.nested[i].saturating_add(frame.nested_ns);
                }
            }
            FrameKind::Present => {
                if self.present_depth != 0 {
                    self.present_depth -= 1;
                    let mut record = self.presents[self.present_depth];
                    record.total_ns = incl;
                    record.nested_ns = frame.nested_ns;
                    if self
                        .worst_present
                        .as_ref()
                        .is_none_or(|w| record.total_ns > w.total_ns)
                    {
                        self.worst_present = Some(record);
                    }
                    closed.present = Some(record);
                }
            }
        }
        closed
    }

    /// Close the tranche and snapshot it.
    pub fn finish(&mut self, at_ms: u64, drain_ns: u64, publish_us: u64) -> TrancheAnatomy {
        let anatomy = TrancheAnatomy {
            at_ms,
            drain_ns,
            publish_us,
            ns: self.ns,
            n: self.n,
            packets: self.packets,
            top_op: self.top_op,
            top_ns: self.top_ns,
            top_self_ns: self.top_self_ns,
            skew_ns: self.skew_ns,
            skew_n: self.skew_n,
            unclosed: self.depth as u64,
            depth_overflow: self.overflow,
            worst_present: self.worst_present,
        };
        self.active = false;
        self.depth = 0;
        self.present_depth = 0;
        anatomy
    }
}

/// The window's worst tranche and present, and the long-line budgets.
struct Window {
    worst: Option<TrancheAnatomy>,
    long_emitted: u64,
    long_suppressed: u64,
    long_total: u64,
    present_worst: Option<PresentAnatomy>,
    present_emitted: u64,
    present_suppressed: u64,
    present_total: u64,
}

impl Window {
    const fn new() -> Self {
        Self {
            worst: None,
            long_emitted: 0,
            long_suppressed: 0,
            long_total: 0,
            present_worst: None,
            present_emitted: 0,
            present_suppressed: 0,
            present_total: 0,
        }
    }

    /// Keep a finished tranche if it is the window's worst, and return the
    /// `drain_long` line when it is long and the budget allows.
    fn tranche(&mut self, anatomy: TrancheAnatomy) -> Option<String> {
        let line = if anatomy.total_us() >= LONG_TRANCHE_US {
            self.long_total += 1;
            self.long_emitted += 1;
            if self.long_emitted <= LONG_LINES_PER_WINDOW {
                Some(anatomy.render("drain_long"))
            } else {
                self.long_suppressed += 1;
                None
            }
        } else {
            None
        };
        if self
            .worst
            .as_ref()
            .is_none_or(|w| anatomy.total_us() > w.total_us())
        {
            self.worst = Some(anatomy);
        }
        line
    }

    /// The same for one finished present packet.
    fn present(&mut self, present: PresentAnatomy) -> Option<String> {
        let line = if present.total_us() >= LONG_PRESENT_US {
            self.present_total += 1;
            self.present_emitted += 1;
            if self.present_emitted <= LONG_LINES_PER_WINDOW {
                Some(present.render("present_long"))
            } else {
                self.present_suppressed += 1;
                None
            }
        } else {
            None
        };
        if self
            .present_worst
            .as_ref()
            .is_none_or(|w| present.total_ns > w.total_ns)
        {
            self.present_worst = Some(present);
        }
        line
    }

    /// The window's `drain_worst` and `present_worst` lines, and reset it.
    fn take(&mut self, win_ms: u64) -> (Option<String>, Option<String>) {
        let worst = self.worst.take().map(|worst| {
            format!(
                "{} win_ms={win_ms} long={} long_suppressed={} long_us={LONG_TRANCHE_US}",
                worst.render("drain_worst"),
                self.long_total,
                self.long_suppressed,
            )
        });
        self.long_emitted = 0;
        self.long_total = 0;
        self.long_suppressed = 0;
        let present = self.present_worst.take().map(|present| {
            format!(
                "{} win_ms={win_ms} long={} long_suppressed={} long_us={LONG_PRESENT_US}",
                present.render("present_worst"),
                self.present_total,
                self.present_suppressed,
            )
        });
        self.present_emitted = 0;
        self.present_total = 0;
        self.present_suppressed = 0;
        (worst, present)
    }
}

/// The process's window. The running tranche is thread-local; the window is
/// what the census thread reads, and the drain worker is the only writer.
static WINDOW: parking_lot::Mutex<Window> = parking_lot::Mutex::new(Window::new());

thread_local! {
    /// The tranche this thread is running, if any. Only this thread's charges
    /// are the tranche's; see the module doc.
    static LEDGER: RefCell<TrancheLedger> = const { RefCell::new(TrancheLedger::new()) };
}

/// The span clock: nanoseconds since the first span this process timed. A
/// monotonic `Instant`, like the worker's own `drain_us` clock, so every span
/// nests inside the interval that clock measures.
fn now_ns() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as u64
}

/// Run `f` on this thread's ledger when it is inside a tranche. Never panics:
/// a ledger already borrowed (which no path does) is skipped, not unwrapped,
/// because these hooks run under the QEMU FFI boundary.
fn with_active<R>(f: impl FnOnce(&mut TrancheLedger) -> R) -> Option<R> {
    LEDGER.with(|ledger| {
        let mut ledger = ledger.try_borrow_mut().ok()?;
        if !ledger.active {
            return None;
        }
        Some(f(&mut ledger))
    })
}

/// A tranche is starting on this thread.
pub(super) fn tranche_begin() {
    LEDGER.with(|ledger| {
        if let Ok(mut ledger) = ledger.try_borrow_mut() {
            ledger.begin();
        }
    });
}

/// The tranche on this thread has ended. Emits `drain_long` when it was long.
pub(super) fn tranche_end(drain_ns: u64, publish_us: u64) {
    let at_ms = crate::observe::elapsed_ms() as u64;
    let Some(anatomy) = with_active(|ledger| ledger.finish(at_ms, drain_ns, publish_us)) else {
        return;
    };
    let line = WINDOW.lock().tranche(anatomy);
    if let Some(line) = line {
        crate::observe::off(line);
    }
}

/// The window's worst tranche and worst present, for the per-second census
/// block.
pub(super) fn take_worst_tranche(win_ms: u64) -> (Option<String>, Option<String>) {
    WINDOW.lock().take(win_ms)
}

/// Charge `ns` to `cost` when this thread is running a tranche.
pub fn note_tranche_cost(cost: TrancheCost, ns: u64) {
    with_active(|ledger| ledger.charge(cost, ns));
}

/// Charge `ns` and `n` events to a cross-cut at once.
pub fn note_tranche_count(cost: TrancheCost, ns: u64, n: u64) {
    with_active(|ledger| ledger.charge_n(cost, ns, n));
}

/// Charge elapsed time since `started` to `cost`.
pub fn note_tranche_since(cost: TrancheCost, started: std::time::Instant) {
    with_active(|ledger| ledger.charge(cost, started.elapsed().as_nanos() as u64));
}

/// An open span, closed when dropped. Inert outside a tranche.
#[must_use = "a span measures until it is dropped"]
pub struct TrancheSpan {
    slot: Option<SpanSlot>,
    /// Spans are thread-local; a guard moved to another thread would close a
    /// span on a stack it was never pushed to.
    _local: std::marker::PhantomData<*const ()>,
}

impl TrancheSpan {
    fn new(slot: Option<SpanSlot>) -> Self {
        Self {
            slot,
            _local: std::marker::PhantomData,
        }
    }
}

impl Drop for TrancheSpan {
    fn drop(&mut self) {
        let Some(slot) = self.slot.take() else {
            return;
        };
        let now = now_ns();
        let closed = with_active(|ledger| ledger.close(slot, now)).flatten();
        if let Some(mut present) = closed.and_then(|c| c.present) {
            present.at_ms = crate::observe::elapsed_ms() as u64;
            let line = WINDOW.lock().present(present);
            if let Some(line) = line {
                crate::observe::off(line);
            }
        }
    }
}

/// Open a span charged exclusive to the tiling cost `cost`.
pub fn tranche_span(cost: TrancheCost) -> TrancheSpan {
    let now = now_ns();
    TrancheSpan::new(with_active(|ledger| ledger.open(cost, now)).flatten())
}

/// Open one step of the present now running. See [`PresentPhase`].
pub fn present_phase(phase: PresentPhase) -> TrancheSpan {
    let now = now_ns();
    TrancheSpan::new(with_active(|ledger| ledger.open_phase(phase, now)).flatten())
}

/// Open one present packet's record. Emits `present_long` on close when long.
pub fn present_scope(channel: u32, mapping: u32) -> TrancheSpan {
    let now = now_ns();
    TrancheSpan::new(with_active(|ledger| ledger.open_present(channel, mapping, now)).flatten())
}

/// One child packet's dispatch. On close it charges `proc` exclusive in the
/// tranche, and feeds the window census (`drain_duty` `proc_us` and its
/// per-opcode split) the same exclusive figure — inclusive only when no tranche
/// is running, where there is nothing nested to separate.
#[must_use = "a span measures until it is dropped"]
pub struct PacketSpan {
    opcode: u16,
    started: std::time::Instant,
    span: TrancheSpan,
}

impl Drop for PacketSpan {
    fn drop(&mut self) {
        let self_ns = self.span.slot.take().and_then(|slot| {
            let now = now_ns();
            with_active(|ledger| ledger.close(slot, now))
                .flatten()
                .and_then(|closed| closed.packet_self_ns)
        });
        let ns = self_ns.unwrap_or_else(|| self.started.elapsed().as_nanos() as u64);
        super::DRAIN_DUTY.note_proc(self.opcode, ns);
    }
}

/// Open one child packet's dispatch span.
pub fn packet_span(opcode: u16) -> PacketSpan {
    let started = std::time::Instant::now();
    let now = now_ns();
    PacketSpan {
        opcode,
        started,
        span: TrancheSpan::new(with_active(|ledger| ledger.open_packet(opcode, now)).flatten()),
    }
}

/// Start a tranche on this thread without the device worker, for a test that
/// drives the drain directly and reads back what the ledger saw.
#[cfg(test)]
pub(crate) fn test_tranche_begin() {
    tranche_begin();
}

/// End the tranche [`test_tranche_begin`] started and return its anatomy,
/// leaving the window untouched. `drain_ns` is the caller's own measurement of
/// the interval it drove.
#[cfg(test)]
pub(crate) fn test_tranche_finish(drain_ns: u64) -> TrancheAnatomy {
    with_active(|ledger| ledger.finish(0, drain_ns, 0)).expect("a test tranche was begun")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ledger inside a tranche, driven by a synthetic clock.
    fn active() -> TrancheLedger {
        let mut l = TrancheLedger::new();
        l.begin();
        l
    }

    fn span(l: &mut TrancheLedger, cost: TrancheCost, at: u64) -> SpanSlot {
        l.open(cost, at).expect("inside a tranche")
    }

    fn close(l: &mut TrancheLedger, slot: SpanSlot, at: u64) -> Closed {
        l.close(slot, at).expect("open")
    }

    /// Every exclusive charge, summed, cannot exceed the interval the spans
    /// nested in — whatever the nesting.
    fn assert_tiles(a: &TrancheAnatomy) {
        assert!(
            a.claimed_ns() <= a.drain_ns,
            "claimed {} > drain {}: {}",
            a.claimed_ns(),
            a.drain_ns,
            a.render("drain_long")
        );
        assert_eq!(a.residue_ns(), Ok(a.drain_ns - a.claimed_ns()));
        assert_eq!((a.skew_n, a.unclosed, a.depth_overflow), (0, 0, 0));
    }

    /// The shape the live RX 7600 line caught: a present packet whose rescue
    /// drain runs a sibling channel's packets. Times in nanoseconds.
    ///
    /// ```text
    /// 0      sweep ............................................ 100_000
    /// 1_000    child_fifo(ch 5) ............................ 95_000
    /// 1_000      setup 500 (leaf)
    /// 2_000      settle ............................... 90_000
    /// 3_000        packet 0x06 ...................... 80_000
    /// 3_000          present ....................... 79_000
    /// 4_000            rescue_child_first ...... 60_000
    /// 5_000              resweep ............ 59_000
    /// 6_000                child_fifo(ch 3) . 58_000
    /// 6_000                  ring 1_000, decode 500 (leaves)
    /// 8_000                  settle ...... 50_000
    /// 9_000                    packet 0x37 .. 40_000   (draw cross-cut 20_000)
    /// 61_000           capture ......... 70_000
    /// ```
    fn nested_present(l: &mut TrancheLedger) {
        let sweep = span(l, TrancheCost::Sweep, 0);
        let outer = span(l, TrancheCost::ChildFifo, 1_000);
        l.charge(TrancheCost::Setup, 500);
        let settle = span(l, TrancheCost::Settle, 2_000);
        let present_packet = l.open_packet(0x06, 3_000).unwrap();
        let present = l.open_present(5, 9, 3_000).unwrap();
        let rescue = l.open_phase(PresentPhase::RescueChildFirst, 4_000).unwrap();
        let resweep = span(l, TrancheCost::Resweep, 5_000);
        let inner = span(l, TrancheCost::ChildFifo, 6_000);
        l.charge(TrancheCost::Ring, 1_000);
        l.charge(TrancheCost::Decode, 500);
        let inner_settle = span(l, TrancheCost::Settle, 8_000);
        let draw_packet = l.open_packet(0x37, 9_000).unwrap();
        l.charge(TrancheCost::Draw, 20_000);
        assert_eq!(close(l, draw_packet, 40_000).packet_self_ns, Some(31_000));
        close(l, inner_settle, 50_000);
        close(l, inner, 58_000);
        close(l, resweep, 59_000);
        close(l, rescue, 60_000);
        let capture = l.open_phase(PresentPhase::Capture, 61_000).unwrap();
        close(l, capture, 70_000);
        let finished = close(l, present, 79_000).present.expect("a present closed");
        assert_eq!(finished.total_ns, 76_000);
        assert_eq!(
            close(l, present_packet, 80_000).packet_self_ns,
            // 77_000 inclusive, less the rescue sweep's 54_000 (which holds
            // the nested child_fifo's 52_000).
            Some(23_000)
        );
        close(l, settle, 90_000);
        close(l, outer, 95_000);
        close(l, sweep, 100_000);
    }

    #[test]
    fn exclusive_tranche_accounting_cannot_exceed_drain_us() {
        let mut l = active();
        nested_present(&mut l);
        let a = l.finish(1, 100_000, 0);
        assert_tiles(&a);
        // Every nanosecond of the outer sweep is claimed by exactly one span.
        assert_eq!(a.claimed_ns(), 100_000);
        assert_eq!(a.residue_ns(), Ok(0));
        // A tranche whose worker measured a little more than the spans: the
        // rest is `other`, and only the rest.
        let mut l = active();
        nested_present(&mut l);
        let a = l.finish(1, 100_250, 0);
        assert_eq!(a.residue_ns(), Ok(250));
    }

    #[test]
    fn nested_child_processing_does_not_double_count_proc() {
        let mut l = active();
        nested_present(&mut l);
        let a = l.finish(1, 100_000, 0);
        // The old inclusive accounting charged 77_000 + 31_000 = 108_000 of
        // `proc` to a 100_000 tranche. Exclusive: the present's own 23_000 and
        // the nested draw packet's 31_000, each once.
        assert_eq!(a.ns_of(TrancheCost::Proc), 23_000 + 31_000);
        assert_eq!(a.count(TrancheCost::Proc), 2);
        assert_eq!(a.packets, 2);
        // The top packet is named by what it cost the guest, and its own share
        // is beside it.
        assert_eq!((a.top_op, a.top_ns, a.top_self_ns), (0x06, 77_000, 23_000));
        // The nested drain is reported once, as a cross-cut.
        assert_eq!(a.ns_of(TrancheCost::NestedDrain), 52_000);
        assert_eq!(a.count(TrancheCost::NestedDrain), 1);
        // And its pieces land in the tiling under their own names.
        assert_eq!(a.ns_of(TrancheCost::Ring), 1_000);
        assert_eq!(a.ns_of(TrancheCost::Decode), 500);
        assert_eq!(a.ns_of(TrancheCost::Setup), 500);
        // inner child_fifo: 52_000 - ring - decode - inner settle 42_000.
        // outer child_fifo: 94_000 - setup 500 - outer settle 88_000.
        assert_eq!(a.ns_of(TrancheCost::ChildFifo), 8_500 + 5_500);
        // outer settle: 88_000 - packet 77_000; inner: 42_000 - packet 31_000.
        assert_eq!(a.ns_of(TrancheCost::Settle), 11_000 + 11_000);
        assert_eq!(a.ns_of(TrancheCost::Resweep), 54_000 - 52_000);
        assert_eq!(a.ns_of(TrancheCost::Sweep), 100_000 - 94_000);
        // A cross-cut inside `proc` is not part of the tiling.
        assert_eq!(a.ns_of(TrancheCost::Draw), 20_000);
        assert_tiles(&a);
    }

    #[test]
    fn the_new_drain_phases_account_for_the_old_other() {
        // `drain_pending`'s top-level work, none of which the old hooks
        // reached: the old line would have read `other_us` = all of it.
        let mut l = active();
        let sweep = span(&mut l, TrancheCost::Sweep, 0);
        let mut at = 0;
        for (cost, len) in [
            (TrancheCost::XlateRetry, 1_000),
            (TrancheCost::MainFifo, 2_000),
            (TrancheCost::Settle, 300_000),
            (TrancheCost::Iosfc, 4_000),
            (TrancheCost::DisplayOnline, 5_000),
            (TrancheCost::RetiredViews, 600_000),
            (TrancheCost::RetireLinear, 7_000),
        ] {
            let s = span(&mut l, cost, at);
            // A root packet run by the main FIFO's settle, a stamp read under
            // the top-level settle: both nested, both exclusive.
            if cost == TrancheCost::MainFifo {
                let root = span(&mut l, TrancheCost::RootProc, at + 100);
                close(&mut l, root, at + 600);
            }
            if cost == TrancheCost::Settle {
                let stamps = span(&mut l, TrancheCost::SettleStamps, at + 10);
                close(&mut l, stamps, at + 200_010);
            }
            at += len;
            close(&mut l, s, at);
        }
        close(&mut l, sweep, at + 50);
        let a = l.finish(1, at + 50, 0);
        assert_tiles(&a);
        assert_eq!(a.residue_ns(), Ok(0), "{}", a.render("drain_long"));
        assert_eq!(a.ns_of(TrancheCost::Sweep), 50);
        assert_eq!(a.ns_of(TrancheCost::RootProc), 500);
        assert_eq!(a.ns_of(TrancheCost::MainFifo), 1_500);
        assert_eq!(a.ns_of(TrancheCost::SettleStamps), 200_000);
        assert_eq!(a.ns_of(TrancheCost::Settle), 100_000);
        assert_eq!(a.ns_of(TrancheCost::RetiredViews), 600_000);
        let line = a.render("drain_long");
        for field in [
            " xlate_retry_us=1 ",
            " main_fifo_us=1 ",
            " root_proc_us=0 root_proc_n=1",
            " settle_us=100 ",
            " settle_stamps_us=200 ",
            " iosfc_us=4 ",
            " display_online_us=5 ",
            " retired_views_us=600 ",
            " retire_linear_us=7 ",
            " other_us=0 ",
        ] {
            assert!(line.contains(field), "{field} missing from {line}");
        }
    }

    #[test]
    fn an_overrun_is_reported_not_clamped() {
        // Leaf charges the worker's own clock did not contain — which the spans
        // make impossible — must say so on the line instead of printing a clean
        // `other_us=0`.
        let mut l = active();
        l.charge(TrancheCost::Tail, 30_000);
        l.charge(TrancheCost::Boundary, 25_000);
        let a = l.finish(1, 50_000, 0);
        assert_eq!(a.residue_ns(), Err(5_000));
        let line = a.render("drain_long");
        assert!(line.contains(" other_us=0 overrun_us=5 "), "{line}");
    }

    #[test]
    fn present_subphase_accounting() {
        let mut l = active();
        nested_present(&mut l);
        let a = l.finish(1, 100_000, 0);
        let p = a.worst_present.expect("the tranche held a present");
        assert_eq!((p.channel, p.mapping), (5, 9));
        assert_eq!(p.total_ns, 76_000);
        // Inclusive steps, the nested drain beside the one that ran it.
        assert_eq!(p.ns[PresentPhase::RescueChildFirst.index()], 56_000);
        assert_eq!(p.nested[PresentPhase::RescueChildFirst.index()], 52_000);
        assert_eq!(p.ns[PresentPhase::Capture.index()], 9_000);
        assert_eq!(p.nested[PresentPhase::Capture.index()], 0);
        assert_eq!(p.nested_ns, 52_000);
        assert_eq!(p.residue_ns(), Ok(76_000 - 56_000 - 9_000));
        let line = p.render("present_long");
        for field in [
            " ch=5 mid=9 total_us=76 self_us=11 nested_us=52",
            " rescue_child_first_us=56 rescue_child_first_n=1 rescue_child_first_nested_us=52",
            " capture_us=9 capture_n=1",
        ] {
            assert!(line.contains(field), "{field} missing from {line}");
        }
        assert!(!line.contains("capture_nested"), "{line}");
        assert!(!line.contains("rescue_main"), "{line}");
        // The steps are transparent to the tiling: the present's own steps did
        // not take anything away from `proc` or the nested drain's costs.
        assert_tiles(&a);
    }

    #[test]
    fn a_present_inside_a_present_charges_its_own_record() {
        let mut l = active();
        let outer_packet = l.open_packet(0x08, 0).unwrap();
        let outer = l.open_present(5, 1, 0).unwrap();
        let rescue = l.open_phase(PresentPhase::RescueChildFirst, 10).unwrap();
        let drain = span(&mut l, TrancheCost::ChildFifo, 20);
        let inner_packet = l.open_packet(0x06, 30).unwrap();
        let inner = l.open_present(3, 2, 30).unwrap();
        let capture = l.open_phase(PresentPhase::Capture, 40).unwrap();
        close(&mut l, capture, 90);
        let inner_done = close(&mut l, inner, 100).present.unwrap();
        close(&mut l, inner_packet, 100);
        close(&mut l, drain, 110);
        close(&mut l, rescue, 120);
        let outer_done = close(&mut l, outer, 130).present.unwrap();
        close(&mut l, outer_packet, 130);
        assert_eq!(inner_done.mapping, 2);
        assert_eq!(inner_done.ns[PresentPhase::Capture.index()], 50);
        assert_eq!(outer_done.mapping, 1);
        assert_eq!(outer_done.ns[PresentPhase::Capture.index()], 0);
        assert_eq!(outer_done.ns[PresentPhase::RescueChildFirst.index()], 110);
        assert_eq!(
            outer_done.nested[PresentPhase::RescueChildFirst.index()],
            90
        );
        let a = l.finish(1, 130, 0);
        assert_eq!(a.worst_present.unwrap().mapping, 1);
        assert_tiles(&a);
    }

    #[test]
    fn a_guard_closed_out_of_order_closes_what_it_enclosed() {
        let mut l = active();
        let a = span(&mut l, TrancheCost::Settle, 0);
        let b = span(&mut l, TrancheCost::Admit, 10);
        // `a` closes first: `b` is closed at the same instant, then `a`.
        close(&mut l, a, 100);
        assert!(l.close(b, 200).is_none(), "b's guard is stale");
        // A new span in b's old slot is not closable through b's stale guard.
        let c = span(&mut l, TrancheCost::Sweep, 300);
        let d = span(&mut l, TrancheCost::Iosfc, 310);
        assert!(l.close(b, 320).is_none());
        close(&mut l, d, 330);
        close(&mut l, c, 400);
        let a = l.finish(1, 400, 0);
        assert_eq!(a.ns_of(TrancheCost::Admit), 90);
        assert_eq!(a.ns_of(TrancheCost::Settle), 10);
        assert_tiles(&a);
    }

    #[test]
    fn begin_forgets_the_previous_tranche() {
        let mut l = active();
        l.charge(TrancheCost::DebtPay, 9_000_000);
        let p = l.open_packet(0x37, 0).unwrap();
        l.close(p, 9_000_000);
        let dangling = span(&mut l, TrancheCost::Settle, 0);
        l.begin();
        assert!(l.close(dangling, 10).is_none());
        let a = l.finish(1, 10, 0);
        assert_eq!(a.us(TrancheCost::DebtPay), 0);
        assert_eq!(a.packets, 0);
        assert_eq!(a.top_ns, 0);
        assert_eq!(a.unclosed, 0);
    }

    #[test]
    fn nothing_charges_outside_a_tranche() {
        let mut l = TrancheLedger::new();
        l.charge(TrancheCost::Ring, 5);
        assert!(l.open(TrancheCost::Settle, 0).is_none());
        assert!(l.open_present(1, 2, 0).is_none());
        l.begin();
        let a = l.finish(1, 10, 0);
        assert_eq!(a.claimed_ns(), 0);
    }

    #[test]
    fn spans_past_the_depth_bound_are_counted_and_fold_into_their_parent() {
        let mut l = active();
        let slots: Vec<_> = (0..MAX_DEPTH as u64)
            .map(|i| span(&mut l, TrancheCost::ChildFifo, i))
            .collect();
        assert!(l.open(TrancheCost::Settle, 100).is_none());
        for (i, slot) in slots.into_iter().enumerate().rev() {
            close(&mut l, slot, 1_000 - i as u64);
        }
        let a = l.finish(1, 1_000, 0);
        assert_eq!(a.depth_overflow, 1);
        assert_eq!(a.claimed_ns(), 1_000);
    }

    #[test]
    fn the_top_packet_is_the_most_expensive_one() {
        let mut l = active();
        for (op, start, end) in [
            (0x10, 0, 5_000),
            (0x37, 5_000, 95_000),
            (0x12, 95_000, 102_000),
        ] {
            let p = l.open_packet(op, start).unwrap();
            l.close(p, end);
        }
        let a = l.finish(1, 102_000, 0);
        assert_tiles(&a);
        assert_eq!((a.top_op, a.top_ns, a.packets), (0x37, 90_000, 3));
        assert_eq!(a.count(TrancheCost::Proc), 3);
        assert_eq!(a.ns_of(TrancheCost::Proc), 102_000);
    }

    fn finished(drain_us: u64, debt_ns: u64) -> TrancheAnatomy {
        let mut l = active();
        l.charge(TrancheCost::DebtPay, debt_ns);
        l.finish(1, drain_us * 1000, 0)
    }

    #[test]
    fn the_window_keeps_its_longest_tranche_and_resets_on_take() {
        let mut w = Window::new();
        for (drain, pay_ns) in [(8_000, 1_000), (97_000, 80_000_000), (12_000, 2_000)] {
            let _ = w.tranche(finished(drain, pay_ns));
        }
        let (line, present) = w.take(1000);
        let line = line.expect("three tranches finished");
        assert!(present.is_none(), "no present ran");
        assert!(line.starts_with("drain_worst "), "{line}");
        assert!(line.contains(" total_us=97000 "), "{line}");
        assert!(line.contains(" debt_pay_us=80000 debt_pay_n=1"), "{line}");
        assert!(line.contains(" long=1 long_suppressed=0 "), "{line}");
        assert!(w.take(1000).0.is_none(), "the window was reset");
    }

    #[test]
    fn long_tranches_print_at_once_up_to_the_budget() {
        let mut w = Window::new();
        let mut printed = 0;
        for _ in 0..LONG_LINES_PER_WINDOW + 3 {
            if let Some(line) = w.tranche(finished(LONG_TRANCHE_US, 0)) {
                assert!(line.starts_with("drain_long t=1 "), "{line}");
                printed += 1;
            }
        }
        assert!(
            w.tranche(finished(LONG_TRANCHE_US - 1, 0)).is_none(),
            "below the threshold"
        );
        assert_eq!(printed, LONG_LINES_PER_WINDOW);
        let line = w.take(1000).0.expect("finished");
        assert!(
            line.contains(&format!(
                " long={} long_suppressed=3 ",
                LONG_LINES_PER_WINDOW + 3
            )),
            "{line}"
        );
        assert!(
            w.tranche(finished(LONG_TRANCHE_US, 0)).is_some(),
            "the budget is per window"
        );
    }

    #[test]
    fn long_presents_print_at_once_and_the_worst_rides_the_window() {
        let mut w = Window::new();
        let present = |total_ns| PresentAnatomy {
            mapping: 4,
            total_ns,
            ..PresentAnatomy::EMPTY
        };
        assert!(w.present(present(LONG_PRESENT_US * 1000 - 1)).is_none());
        let line = w.present(present(628_000_000)).expect("long");
        assert!(
            line.starts_with("present_long t=0 ch=0 mid=4 total_us=628000 "),
            "{line}"
        );
        let (_, worst) = w.take(1000);
        let worst = worst.expect("two presents finished");
        assert!(worst.starts_with("present_worst "), "{worst}");
        assert!(worst.contains(" total_us=628000 "), "{worst}");
        assert!(worst.contains(" long=1 long_suppressed=0 "), "{worst}");
    }

    #[test]
    fn a_line_names_only_what_the_tranche_did() {
        let mut l = active();
        l.charge(TrancheCost::PipeCreate, 120_000_000);
        let line = l.finish(1, 130_000_000, 0).render("drain_long");
        assert!(
            line.contains(" pipe_create_us=120000 pipe_create_n=1"),
            "{line}"
        );
        assert!(!line.contains("rb_fence"), "{line}");
        assert!(!line.contains("present_us"), "{line}");
    }

    #[test]
    fn charges_off_the_tranche_thread_are_ignored() {
        // The real hooks: a tranche begun on this thread is not visible from
        // another, so that thread's charges and spans are inert.
        test_tranche_begin();
        std::thread::spawn(|| {
            note_tranche_cost(TrancheCost::Ring, 1_000_000);
            let _span = tranche_span(TrancheCost::Settle);
        })
        .join()
        .expect("thread");
        let a = test_tranche_finish(1);
        assert_eq!(a.claimed_ns(), 0);
    }

    #[test]
    fn labels_are_unique_and_all_is_complete() {
        let mut seen = std::collections::BTreeSet::new();
        for (i, c) in TrancheCost::ALL.iter().enumerate() {
            assert_eq!(c.index(), i);
            assert!(seen.insert(c.label()), "duplicate label {}", c.label());
            assert_eq!(c.is_tiling(), TrancheCost::TILING.contains(c));
        }
        for (i, p) in PresentPhase::ALL.iter().enumerate() {
            assert_eq!(p.index(), i);
            assert!(seen.insert(p.label()), "duplicate label {}", p.label());
        }
    }
}
