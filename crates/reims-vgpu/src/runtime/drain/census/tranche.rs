//! Compile-away compatibility surface for the removed drain-tranche profiler.
//!
//! The original ledger timed every nested drain/admission/present span, kept a
//! per-tranche stack, aggregated long/worst records and emitted several census
//! lines. None of those readings participate in guest ordering or execution.
//! The aggressive cut keeps the vocabulary so functional call sites stay
//! readable while every hook is a zero-cost no-op after inlining.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrancheCost {
    Setup,
    Ring,
    Decode,
    Regs,
    Proc,
    Tail,
    Boundary,
    RootProc,
    XlateRetry,
    MainFifo,
    Sweep,
    Resweep,
    ChildFifo,
    Admit,
    AdmitStampWaits,
    AdmitArrival,
    AdmitReadExec,
    AdmitBuildPacket,
    AdmitWaitFilter,
    AdmitAccessModes,
    AdmitPipelineDeclare,
    AdmitPreflight,
    AdmitPipelineState,
    AdmitModel,
    AdmitBuildParked,
    AdmitPark,
    Settle,
    SettleStamps,
    SettlePump,
    Complete,
    Iosfc,
    DisplayOnline,
    RetiredViews,
    RetireLinear,
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
    pub const fn label(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Ring => "ring",
            Self::Decode => "decode",
            Self::Regs => "regs",
            Self::Proc => "proc",
            Self::Tail => "tail",
            Self::Boundary => "boundary",
            Self::RootProc => "root_proc",
            Self::XlateRetry => "xlate_retry",
            Self::MainFifo => "main_fifo",
            Self::Sweep => "sweep",
            Self::Resweep => "resweep",
            Self::ChildFifo => "child_fifo",
            Self::Admit => "admit",
            Self::AdmitStampWaits => "admit_stamp_waits",
            Self::AdmitArrival => "admit_arrival",
            Self::AdmitReadExec => "admit_read_exec_submission",
            Self::AdmitBuildPacket => "admit_build_packet",
            Self::AdmitWaitFilter => "admit_wait_filter",
            Self::AdmitAccessModes => "admit_access_modes",
            Self::AdmitPipelineDeclare => "admit_pipeline_declare",
            Self::AdmitPreflight => "admit_preflight",
            Self::AdmitPipelineState => "admit_pipeline_state",
            Self::AdmitModel => "admit_model",
            Self::AdmitBuildParked => "admit_build_parked_work",
            Self::AdmitPark => "admit_park",
            Self::Settle => "settle",
            Self::SettleStamps => "settle_stamps",
            Self::SettlePump => "settle_pump",
            Self::Complete => "complete",
            Self::Iosfc => "iosfc",
            Self::DisplayOnline => "display_online",
            Self::RetiredViews => "retired_views",
            Self::RetireLinear => "retire_linear",
            Self::Draw => "draw",
            Self::Compute => "compute",
            Self::FlushRender => "flush_render",
            Self::FlushGva => "flush_gva",
            Self::FlushLinear => "flush_linear",
            Self::FlushStorage => "flush_storage",
            Self::RbSubmit => "rb_submit",
            Self::RbFence => "rb_fence",
            Self::RbMap => "rb_map",
            Self::RbWrite => "rb_write",
            Self::RbVouch => "rb_vouch",
            Self::RbResolve => "rb_resolve",
            Self::RbGpuBar => "rb_gpu_bar",
            Self::RbGpuCopy => "rb_gpu_copy",
            Self::ChPipeline => "ch_pipeline",
            Self::ChAir => "ch_air",
            Self::ChXlate => "ch_xlate",
            Self::ChBinds => "ch_binds",
            Self::ChSampled => "ch_sampled",
            Self::ChSeed => "ch_seed",
            Self::ChAssemble => "ch_assemble",
            Self::ChEngine => "ch_engine",
            Self::ChStore => "ch_store",
            Self::PipeCreate => "pipe_create",
            Self::RingWait => "ring_wait",
            Self::EntryWait => "entry_wait",
            Self::LockWait => "lock_wait",
            Self::DebtPay => "debt_pay",
            Self::Overlay => "overlay",
            Self::Alloc => "alloc",
            Self::NestedDrain => "nested_drain",
            Self::PumpPreflight => "pump_preflight",
            Self::SettleWalk => "settle_walk",
            Self::HostMap => "host_map",
            Self::HostUnmap => "host_unmap",
            Self::HostTrack => "host_track",
            Self::HostUntrack => "host_untrack",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentPhase {
    RescueChildFirst,
    RescueChildSecond,
    RescueMain,
    RescueChildAfterMain,
    PayCpuShared,
    EnsureSurface,
    ResolveBacking,
    ResidentCarries,
    FieldWitness,
    Capture,
    ContentStats,
    EnqueueScanout,
    SignalComplete,
}

impl PresentPhase {
    pub const fn label(self) -> &'static str {
        match self {
            Self::RescueChildFirst => "rescue_child_first",
            Self::RescueChildSecond => "rescue_child_second",
            Self::RescueMain => "rescue_main",
            Self::RescueChildAfterMain => "rescue_child_after_main",
            Self::PayCpuShared => "pay_cpu_shared",
            Self::EnsureSurface => "ensure_surface",
            Self::ResolveBacking => "resolve_backing",
            Self::ResidentCarries => "resident_carries",
            Self::FieldWitness => "field_witness",
            Self::Capture => "capture",
            Self::ContentStats => "content_stats",
            Self::EnqueueScanout => "enqueue_scanout",
            Self::SignalComplete => "signal_complete",
        }
    }
}

#[derive(Default)]
pub struct TrancheSpan;
#[derive(Default)]
pub struct AdmissionScope;
#[derive(Default)]
pub struct PacketSpan;

#[inline(always)]
pub fn tranche_span(_cost: TrancheCost) -> TrancheSpan {
    TrancheSpan
}

#[inline(always)]
pub fn present_phase(_phase: PresentPhase) -> TrancheSpan {
    TrancheSpan
}

#[inline(always)]
pub fn admission_scope(_opcode: u16, _channel: Option<u32>) -> AdmissionScope {
    AdmissionScope
}

#[inline(always)]
pub fn present_scope(_channel: u32, _mapping: u32) -> TrancheSpan {
    TrancheSpan
}

#[inline(always)]
pub fn packet_span(_opcode: u16) -> PacketSpan {
    PacketSpan
}

#[inline(always)]
pub fn note_tranche_cost(_cost: TrancheCost, _ns: u64) {}

#[inline(always)]
pub fn note_tranche_count(_cost: TrancheCost, _ns: u64, _n: u64) {}

#[inline(always)]
pub fn note_tranche_since(_cost: TrancheCost, _started: std::time::Instant) {}

#[inline(always)]
pub fn note_hazard_scan(_scan: crate::model::HazardScan) {}
