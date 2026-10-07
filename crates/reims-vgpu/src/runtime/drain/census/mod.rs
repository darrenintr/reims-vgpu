//! Compatibility surface for the removed runtime census.
//!
//! The full profiler used atomics, clocks, per-window maps and nested ledgers
//! throughout the drain/draw/readback hot paths. None of those readings decide
//! guest-visible behaviour. The aggressive cut keeps the vocabulary at call
//! sites while compiling the production hooks down to no-ops.
//!
//! Route counters remain active only under `cfg(test)`: many correctness tests
//! use them to assert which functional arm ran, while production should not pay
//! for that observation.

pub mod stall;
mod tranche;
pub use tranche::{
    admission_scope, note_hazard_scan, note_tranche_cost, note_tranche_count, note_tranche_since,
    packet_span, present_phase, present_scope, tranche_span, AdmissionScope, PacketSpan,
    PresentPhase, TrancheCost, TrancheSpan,
};

pub(crate) const VBL_NOT_ONLINE: usize = 0;
pub(crate) const VBL_NOT_CLAIMED: usize = 1;
pub(crate) const VBL_DELIVERED: usize = 2;
pub(crate) const VBL_NOT_ENABLED: usize = 3;

pub(crate) const DISPLAY_PRESENT_NO_GPA: usize = 0;
pub(crate) const DISPLAY_PRESENT_NOT_ENABLED: usize = 1;
pub(crate) const DISPLAY_PRESENT_DELIVERED: usize = 2;
pub(crate) const DISPLAY_PRESENT_REFRESH: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainPhase {
    Draw,
    Compute,
    Flush(FlushRail),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushRail {
    Render,
    Gva,
    Linear,
    Storage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadbackPhase {
    Submit,
    Fence,
    Map,
    Write,
    Vouch,
    Resolve,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecPhase {
    Load,
    Preflight,
    Walk,
    Finish,
    Header,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreflightPart {
    Air,
    Cache,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishPhase {
    Prelude,
    Retarget,
    Binds,
    Encode,
    Result,
    Tail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegsOp {
    TailRead,
    HeadWrite,
    Stamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowPublish {
    Fresh,
    NoWindow,
    NoFrame,
    SameKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceWritePhase {
    Stage,
    Land,
    Cache,
}

#[derive(Default)]
pub(crate) struct VblCensus;
impl VblCensus {
    #[inline(always)]
    pub(crate) fn note(&self, _arm: usize, _now_ms: u64) -> Option<String> {
        None
    }
}

#[derive(Default)]
pub(crate) struct WindowPublishCensus;
impl WindowPublishCensus {
    #[inline(always)]
    pub(crate) fn note(&self, _arm: WindowPublish) {}
    #[inline(always)]
    pub(crate) fn take(&self, _win_ms: u64) -> Option<String> {
        None
    }
}

#[derive(Default)]
pub(crate) struct SurfaceWriteCensus;
impl SurfaceWriteCensus {
    #[inline(always)]
    pub(crate) fn note(&self, _phase: SurfaceWritePhase, _us: u64) {}
    #[inline(always)]
    pub(crate) fn note_path(&self, _contiguous: bool, _bytes: u64) {}
    #[inline(always)]
    pub(crate) fn take(&self, _win_ms: u64) -> Option<String> {
        None
    }
}

#[derive(Default)]
pub(crate) struct ResidentArmCensus;
impl ResidentArmCensus {
    #[inline(always)]
    pub(crate) fn note_arm(&self, _now_us: u64) {}
    #[inline(always)]
    pub(crate) fn note_flush(&self, _now_us: u64) {}
    #[inline(always)]
    pub(crate) fn take(&self, _win_ms: u64) -> Option<String> {
        None
    }
}

#[derive(Default)]
pub(crate) struct DrainDutyCensus;
impl DrainDutyCensus {
    #[inline(always)]
    pub(crate) fn note_skipped(&self) {}
    #[inline(always)]
    pub(crate) fn note_gap_entry(&self, entry_us: u64) -> u64 {
        entry_us
    }
    #[inline(always)]
    pub(crate) fn note_irq_armed(&self, _now_us: u64) {}
    #[inline(always)]
    pub(crate) fn note_irq_delivered(&self, _now_us: u64) {}
    #[inline(always)]
    pub(crate) fn note_gap_lock(&self, _us: u64) {}
    #[inline(always)]
    pub(crate) fn note_gap_exit(&self, _exit_us: u64, _busy_end_us: u64, _skipped: bool) {}
    #[inline(always)]
    pub(crate) fn note_phase(&self, _phase: DrainPhase, _us: u64) {}
    #[inline(always)]
    pub(crate) fn take_flush_rails(&self) -> Option<String> {
        None
    }
    #[inline(always)]
    pub(crate) fn take_readback_split(&self) -> Option<String> {
        None
    }
    #[inline(always)]
    pub(crate) fn last_window_ms(&self) -> u64 {
        0
    }
    #[inline(always)]
    pub(crate) fn note_readback(&self, _phase: ReadbackPhase, _us: u64) {}
    #[inline(always)]
    pub(crate) fn note_tail(&self, _tail_us: u64, _boundary_us: u64) {}
    #[inline(always)]
    pub(crate) fn note_ring(&self, _ns: u64) {}
    #[inline(always)]
    pub(crate) fn note_decode(&self, _ns: u64) {}
    #[inline(always)]
    pub(crate) fn note_proc(&self, _opcode: u16, _ns: u64) {}
    #[inline(always)]
    pub(crate) fn take_proc_ops(&self) -> Option<String> {
        None
    }
    #[inline(always)]
    pub(crate) fn note_preflight(&self, _part: PreflightPart, _ns: u64) {}
    #[inline(always)]
    pub(crate) fn note_preflight_pipe(&self) {}
    #[inline(always)]
    pub(crate) fn take_preflight_parts(&self) -> Option<String> {
        None
    }
    #[inline(always)]
    pub(crate) fn note_exec(&self, _phase: ExecPhase, _ns: u64) {}
    #[inline(always)]
    pub(crate) fn take_exec_phases(&self) -> Option<String> {
        None
    }
    #[inline(always)]
    pub(crate) fn note_finish(&self, _phase: FinishPhase, _ns: u64, _entries: u64) {}
    #[inline(always)]
    pub(crate) fn take_finish_phases(&self) -> Option<String> {
        None
    }
    #[inline(always)]
    pub(crate) fn note_regs(&self, _op: RegsOp, _ns: u64) {}
    #[inline(always)]
    pub(crate) fn note_setup(&self, _ns: u64) {}
    #[inline(always)]
    pub(crate) fn note(&self, _drain_us: u64, _publish_us: u64, _now_ms: u64) -> Option<String> {
        None
    }
}

#[derive(Default)]
pub(crate) struct VcpuLockCensus;
pub(crate) const UNCONTENDED_POLL: u64 = 1024;
impl VcpuLockCensus {
    #[inline(always)]
    pub(crate) fn note_uncontended(&self, _now_ms: impl FnOnce() -> u64) -> Option<String> {
        None
    }
    #[inline(always)]
    pub(crate) fn note_wait(&self, _us: u64, _now_ms: u64) -> Option<String> {
        None
    }
}

#[derive(Default)]
pub(crate) struct DoorbellCensus;
impl DoorbellCensus {
    #[inline(always)]
    pub(crate) fn note_direct(&self, _now_ms: impl FnOnce() -> u64) -> Option<String> {
        None
    }
    #[inline(always)]
    pub(crate) fn note_lock_free(&self, _now_ms: impl FnOnce() -> u64) -> Option<String> {
        None
    }
    #[inline(always)]
    pub(crate) fn note_queued(&self, _offset: u64, _age_us: u64, _now_ms: u64) -> Option<String> {
        None
    }
}

#[inline(always)]
pub(crate) fn note_vbl(_arm: usize, _now_ms: u64) {}
#[inline(always)]
pub(crate) fn note_irq_coalesced(_kind: crate::runtime::host::HostActionKind) {}
#[inline(always)]
pub(crate) fn note_display_present_signal(_arm: usize) {}
#[inline(always)]
pub(crate) fn note_display_enable_mask(_mask: u32) {}

#[inline(always)]
pub fn note_doorbell_lock_free() {}
#[inline(always)]
pub fn note_doorbell_direct() {}
#[inline(always)]
pub fn note_doorbell_queued(_offset: u64, _age_us: u64) {}
#[inline(always)]
pub fn note_vcpu_lock_free() {}
#[inline(always)]
pub fn note_vcpu_lock_wait(_us: u64) {}
#[inline(always)]
pub fn note_window_publish(_arm: WindowPublish) {}
#[inline(always)]
pub fn note_surface_write_phase(_phase: SurfaceWritePhase, _us: u64) {}
#[inline(always)]
pub fn note_surface_write_path(_contiguous: bool, _bytes: u64) {}
#[inline(always)]
pub fn note_resident_window_armed() {}
#[inline(always)]
pub fn note_resident_window_flushed() {}

#[inline(always)]
pub fn note_tranche_started(_now_us: u64) {}
#[inline(always)]
pub fn tranche_seq() -> u64 {
    0
}
#[inline(always)]
pub fn tranche_elapsed_us() -> u64 {
    0
}
#[inline(always)]
pub fn note_list_lookup_age(_hit: bool, _us: u64) {}

#[inline(always)]
pub fn note_drain_tail(_tail_ns: u64, _boundary_ns: u64) {}
#[inline(always)]
pub fn note_drain_ring(_ns: u64) {}
#[inline(always)]
pub fn note_drain_decode(_ns: u64) {}
#[inline(always)]
pub fn note_exec_phase(_phase: ExecPhase, _ns: u64) {}
#[inline(always)]
pub fn note_finish_phase(_phase: FinishPhase, _ns: u64, _entries: u64) {}
#[inline(always)]
pub fn note_preflight_part(_part: PreflightPart, _ns: u64) {}
#[inline(always)]
pub fn note_preflight_pipe() {}
#[inline(always)]
pub fn note_drain_regs(_op: RegsOp, _ns: u64) {}
#[inline(always)]
pub fn note_drain_setup(_ns: u64) {}

#[inline(always)]
pub fn note_drain_tranche(
    _state: &crate::model::DeviceState,
    _host: &dyn crate::runtime::host::HostOps,
    _drain_ns: u64,
    _publish_us: u64,
) {
}

#[inline(always)]
pub fn note_drain_skipped() {}

#[inline(always)]
pub fn note_drain_entry() -> u64 {
    crate::observe::elapsed_us()
}

#[inline(always)]
pub fn note_drain_lock_wait(_us: u64) {}
#[inline(always)]
pub fn note_irq_armed() {}
#[inline(always)]
pub fn note_irq_delivered() {}
#[inline(always)]
pub fn note_drain_exit(_busy_end_us: u64, _skipped: bool) {}
#[inline(always)]
pub fn note_drain_phase(_phase: DrainPhase, _started: std::time::Instant) {}
#[inline(always)]
pub fn note_readback_phase(_phase: ReadbackPhase, _us: u64) {}
#[inline(always)]
pub fn note_readback_gpu_us(_barrier_us: u64, _copy_us: u64) {}

#[cfg(test)]
static STORE_ROUTES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<&'static str, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

#[inline(always)]
pub fn note_store_route(route: &'static str) {
    #[cfg(test)]
    {
        let mut routes = STORE_ROUTES.lock().unwrap_or_else(|e| e.into_inner());
        *routes.entry(route).or_default() += 1;
    }
    #[cfg(not(test))]
    let _ = route;
}

#[inline(always)]
pub fn note_store_route_n(route: &'static str, n: u64) {
    #[cfg(test)]
    {
        let mut routes = STORE_ROUTES.lock().unwrap_or_else(|e| e.into_inner());
        *routes.entry(route).or_default() += n;
    }
    #[cfg(not(test))]
    let _ = (route, n);
}

#[inline(always)]
pub fn note_store_route_us(name: &'static str, us: u64) {
    #[cfg(test)]
    {
        let mut routes = STORE_ROUTES.lock().unwrap_or_else(|e| e.into_inner());
        *routes.entry(name).or_default() += us;
    }
    #[cfg(not(test))]
    let _ = (name, us);
}

pub(crate) fn store_route_count(route: &str) -> u64 {
    #[cfg(test)]
    {
        return STORE_ROUTES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(route)
            .copied()
            .unwrap_or(0);
    }
    #[cfg(not(test))]
    {
        let _ = route;
        0
    }
}
