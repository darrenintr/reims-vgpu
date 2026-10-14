//! The stamp queue: the FIFO's in-order record of the commands it has started and not yet
//! published.
//!
//! Each command pushes its stamp value before it runs; its handler, or the task it hands the
//! command to, later signals or faults that value. The queue publishes in push order only: a
//! pump hands the device the last value of the contiguous completed prefix (one signal for the
//! prefix), then reports a faulted head once until the device resumes it. 128 stamps may be
//! pending; a push over that waits for a pump.
//!
//! Ordering contracts, each with its test:
//! - a push blocks while 128 are pending, and one pump wakes one pusher
//!   (`push::blocks_at_128_pending`, `push::wakes_when_the_head_completes`, `loom_tests`);
//! - signal and fault find the first pending slot with the value; a value outside the window
//!   marks nothing (`signal::late_second_completion_is_swallowed_or_dies`);
//! - the sink's calls run with the queue's lock held, in pump order (`signal::head_signal_publishes`);
//! - the fault latch reports a faulted head once until a resume
//!   (`pump::fault_is_reported_once_per_latch`, `resume_from_fault::*`);
//! - a quiesced pump counts a faulted slot as completed (`pump::quiesced_consumes_faulted_slots`);
//! - drain waits for every pending stamp, and hangs on one that never completes
//!   (`drain::hangs_on_a_pending_stamp`).

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Weak};

use imp::{Condvar, Mutex};

use crate::fatal_unless;
use crate::fifo::{DeathKind, Fifo};
use crate::num::ToUsize;
use crate::platform::Platform;

/// How many stamps may be pending.
pub const STAMP_QUEUE_CAPACITY: u32 = 128;

/// How many pending slots [`StampQueue::describe_pending`] shows.
const DESCRIBE_SHOWN: u32 = 10;

/// "No fault to report".
const NO_FAULT: u32 = u32::MAX;

/// The queue's callbacks: called with the queue's lock held, in pump order.
pub trait StampSink: Send + Sync {
    /// The completed prefix ends at `stamp_value`.
    fn signal(&self, stamp_value: u32);
    /// The head is faulted at `fault_offset`.
    fn fault(&self, fault_offset: u32);
    /// A resume found its head slot not faulted: the queue's one log line (diagnostics); the
    /// default logs nothing.
    fn head_not_faulted(&self) {}
}

/// One pending slot: the value, the command ID, and the flags word (bit 0 signalled, bit 1
/// faulted, the fault offset in the 30 bits above them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PendingStamp {
    /// The stamp value.
    pub stamp_value: u32,
    /// The command ID (the header's `u16`, widened).
    pub command_id: u32,
    /// The flags word.
    pub flags: u32,
}

impl PendingStamp {
    /// Bit 0: signalled.
    pub const fn signaled(&self) -> bool {
        self.flags & 1 != 0
    }

    /// Bit 1: faulted.
    pub const fn faulted(&self) -> bool {
        self.flags & 2 != 0
    }

    /// Bits 2-31: the fault offset.
    pub const fn offset(&self) -> u32 {
        self.flags.wrapping_shr(2)
    }

    fn set_signaled(&mut self, v: bool) {
        self.flags = (self.flags & !1) | u32::from(v);
    }

    fn set_faulted(&mut self, v: bool) {
        self.flags = (self.flags & !2) | u32::from(v).wrapping_shl(1);
    }

    /// The offset field is 30 bits wide, so bits 30 and 31 of `v` are lost.
    const fn set_offset(&mut self, v: u32) {
        self.flags = (self.flags & 3) | v.wrapping_shl(2);
    }
}

/// The queue's state, under its lock.
#[derive(Debug)]
struct QueueState {
    /// The ring of slots, zeroed at first; completed slots keep their contents.
    slots: [PendingStamp; 128],
    /// Stamps pushed (wrapping).
    write_count: u32,
    /// Stamps published (wrapping).
    complete_count: u32,
    /// A faulted head has been reported and not yet resumed.
    faulted: bool,
    /// The FIFO is quiesced: a pump counts a faulted slot as completed.
    quiesced: bool,
}

impl QueueState {
    const fn pending_count(&self) -> u32 {
        self.write_count.wrapping_sub(self.complete_count)
    }

    /// The slot of logical index `index`.
    #[allow(clippy::indexing_slicing)] // index % 128 < 128 == slots.len()
    fn slot(&mut self, index: u32) -> &mut PendingStamp {
        &mut self.slots
            [(index % const { NonZeroU32::new(STAMP_QUEUE_CAPACITY).unwrap() }).to_usize()]
    }
}

/// The production primitives: `parking_lot`'s.
#[cfg(not(loom))]
mod imp {
    pub(super) use parking_lot::{Condvar, Mutex};

    use super::QueueState;

    pub(super) type Guard<'a> = parking_lot::MutexGuard<'a, QueueState>;

    pub(super) fn lock(m: &Mutex<QueueState>) -> Guard<'_> {
        m.lock()
    }

    /// One wait, no predicate: the callers loop.
    pub(super) fn wait<'a>(cv: &Condvar, mut g: Guard<'a>) -> Guard<'a> {
        cv.wait(&mut g);
        g
    }
}

/// `loom`'s primitives, for the model check.
#[cfg(loom)]
mod imp {
    pub(super) use loom::sync::{Condvar, Mutex};

    use super::QueueState;

    pub(super) type Guard<'a> = loom::sync::MutexGuard<'a, QueueState>;

    /// Nothing panics while holding it in the model, so it never poisons.
    pub(super) fn lock(m: &Mutex<QueueState>) -> Guard<'_> {
        m.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// One wait, no predicate: the callers loop.
    pub(super) fn wait<'a>(cv: &Condvar, g: Guard<'a>) -> Guard<'a> {
        cv.wait(g).unwrap_or_else(|p| p.into_inner())
    }
}

/// 128 slots, the pump and the fault latch.
///
/// Dropping it drops the sink and its share of the device's last-pending cell; it publishes
/// nothing and wakes no one.
pub struct StampQueue {
    state: Mutex<QueueState>,
    /// Wakes a push waiting for room.
    room: Condvar,
    /// Wakes a drain waiting for the last stamp.
    drained: Condvar,
    /// The device's record of the last value pushed on this FIFO.
    last_pending: Arc<AtomicU32>,
    sink: Box<dyn StampSink>,
}

impl std::fmt::Debug for StampQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StampQueue")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// Where the scan of a signal or a fault found the value.
enum Found {
    /// Not in the pending window.
    Nowhere,
    /// At this logical index.
    At(u32),
}

impl StampQueue {
    /// An empty queue publishing to `sink`, recording each pushed value in `last_pending`.
    pub fn new(last_pending: Arc<AtomicU32>, sink: Box<dyn StampSink>) -> StampQueue {
        StampQueue {
            state: Mutex::new(QueueState {
                slots: [PendingStamp::default(); 128],
                write_count: 0,
                complete_count: 0,
                faulted: false,
                quiesced: false,
            }),
            room: Condvar::new(),
            drained: Condvar::new(),
            last_pending,
            sink,
        }
    }

    /// Pushes a pending stamp, waiting while 128 are pending. A quiesced queue still takes it.
    pub fn push_stamp(&self, stamp_value: u32, command_id: u32) {
        let mut st = imp::lock(&self.state);
        if st.pending_count() == STAMP_QUEUE_CAPACITY {
            // Room comes as work completes: what the thread held back goes first, unlocked.
            drop(st);
            crate::rt::thread::before_block();
            st = imp::lock(&self.state);
        }
        while st.pending_count() == STAMP_QUEUE_CAPACITY {
            st = imp::wait(&self.room, st);
        }
        let write = st.write_count;
        let slot = st.slot(write);
        slot.stamp_value = stamp_value;
        slot.command_id = command_id;
        slot.flags = 0;
        st.write_count = write.wrapping_add(1);
        // Relaxed, with the lock still held
        self.last_pending.store(stamp_value, Ordering::Relaxed);
    }

    /// The first pending slot with the value, and whether every slot before it (or every slot,
    /// when none has it) is signalled.
    fn scan(st: &mut QueueState, stamp_value: u32) -> (Found, bool) {
        let mut at_head = true;
        let mut index = st.complete_count;
        while index != st.write_count {
            let slot = *st.slot(index);
            if slot.stamp_value != stamp_value {
                // A faulted, unsignalled slot also clears it
                if !slot.signaled() {
                    at_head = false;
                }
                index = index.wrapping_add(1);
                continue;
            }
            return (Found::At(index), at_head);
        }
        (Found::Nowhere, at_head)
    }

    /// Marks the first pending slot with the value signalled, then pumps if everything before
    /// it is.
    pub fn signal_stamp(&self, stamp_value: u32) {
        let mut st = imp::lock(&self.state);
        fatal_unless!(st.pending_count() != 0, DeathKind::SignalNothingPending);
        let (found, at_head) = Self::scan(&mut st, stamp_value);
        if let Found::At(index) = found {
            let slot = st.slot(index);
            // A duplicate completion, or completing a faulted stamp, dies.
            fatal_unless!(!slot.signaled(), DeathKind::SignalTwice);
            fatal_unless!(!slot.faulted(), DeathKind::SignalFaulted);
            slot.set_signaled(true);
        }
        // A value not in the window (a late second completion) marks nothing.
        if at_head {
            self.pump(&mut st);
        }
    }

    /// Marks the first pending slot with the value faulted at `offset`, then pumps if everything
    /// before it is signalled.
    pub fn fault_stamp(&self, stamp_value: u32, offset: u32) {
        let mut st = imp::lock(&self.state);
        fatal_unless!(st.pending_count() != 0, DeathKind::FaultNothingPending);
        // Offsets of 2^31 and up die; 2^30 and up lose their top bits.
        fatal_unless!(offset < 0x8000_0000, DeathKind::FaultOffsetRange);
        let (found, at_head) = Self::scan(&mut st, stamp_value);
        if let Found::At(index) = found {
            let slot = st.slot(index);
            // A fault of a signalled or already faulted stamp dies.
            fatal_unless!(!slot.signaled(), DeathKind::FaultSignalled);
            fatal_unless!(!slot.faulted(), DeathKind::FaultTwice);
            slot.set_faulted(true);
            slot.set_offset(offset);
        }
        if at_head {
            self.pump(&mut st);
        }
    }

    /// Resumes a faulted head: it counts as signalled and the latch clears. Then a pump.
    pub fn resume_from_fault(&self) {
        let mut st = imp::lock(&self.state);
        // The head slot even when nothing is pending: a stale slot a quiesced pump consumed
        // keeps its faulted bit, and is "resumed" here.
        let complete = st.complete_count;
        let slot = st.slot(complete);
        if slot.faulted() {
            slot.set_faulted(false);
            slot.set_signaled(true);
            st.faulted = false;
        } else {
            // The latch stays as it is.
            self.sink.head_not_faulted();
        }
        self.pump(&mut st);
    }

    /// With the lock held: publishes the contiguous completed prefix (one signal, of its last
    /// value), then reports a faulted head once per latch.
    fn pump(&self, st: &mut QueueState) {
        let pending_count = st.pending_count();
        let mut signaled_count: u32 = 0;
        let mut stamp_value: u32 = 0;
        let mut fault_offset = NO_FAULT;
        let mut index = st.complete_count;
        while signaled_count < pending_count {
            let pending = *st.slot(index);
            index = index.wrapping_add(1);
            if !pending.signaled() {
                if !pending.faulted() {
                    break;
                }
                // Quiesced, a faulted slot counts as completed and the latch is left set.
                if !st.quiesced {
                    if !st.faulted {
                        fault_offset = pending.offset();
                    }
                    break;
                }
            }
            stamp_value = pending.stamp_value;
            signaled_count = signaled_count.wrapping_add(1);
        }
        if signaled_count != 0 {
            // Before the count moves, with the lock held
            self.sink.signal(stamp_value);
            st.complete_count = st.complete_count.wrapping_add(signaled_count);
            self.room.notify_one();
        }
        if fault_offset != NO_FAULT {
            st.faulted = true;
            self.sink.fault(fault_offset);
        }
        if st.write_count == st.complete_count {
            self.drained.notify_one();
        }
    }

    /// Quiesces the queue, then pumps.
    pub fn quiesce(&self) {
        let mut st = imp::lock(&self.state);
        st.quiesced = true;
        self.pump(&mut st);
    }

    /// Clears the quiesced flag: no pump, no wake, and the fault latch as it is.
    pub fn clear_quiesced(&self) {
        let mut st = imp::lock(&self.state);
        st.quiesced = false;
    }

    /// Pumps, then waits for every pending stamp (a hang if one never completes).
    pub fn drain(&self) {
        let mut st = imp::lock(&self.state);
        fatal_unless!(st.quiesced, DeathKind::QueueDrainUnquiesced);
        self.pump(&mut st);
        while st.write_count != st.complete_count {
            st = imp::wait(&self.drained, st);
        }
    }

    /// Diagnostics: the first 10 pending slots, then how many more.
    pub fn describe_pending(&self) -> String {
        use std::fmt::Write as _;
        let mut st = imp::lock(&self.state);
        let pending_count = st.pending_count();
        let shown = pending_count.min(DESCRIBE_SHOWN);
        let mut description = String::new();
        let complete = st.complete_count;
        let mut i: u32 = 0;
        while i < shown {
            let pending = *st.slot(complete.wrapping_add(i));
            let _ = writeln!(
                description,
                "\tCMD=0x{:x} signalStamp=0x{:x} signaled={} faulted={}",
                pending.command_id,
                pending.stamp_value,
                u32::from(pending.signaled()),
                u32::from(pending.faulted()),
            );
            i = i.wrapping_add(1);
        }
        if pending_count > DESCRIBE_SHOWN {
            let _ = write!(
                description,
                "\tAnd {} more commands",
                pending_count.wrapping_sub(DESCRIBE_SHOWN).cast_signed()
            );
        }
        description
    }

    /// The queue's whole state.
    #[cfg(test)]
    pub fn snapshot(&self) -> StampQueueSnapshot {
        let st = imp::lock(&self.state);
        StampQueueSnapshot {
            slots: st.slots,
            write_count: st.write_count,
            complete_count: st.complete_count,
            faulted: st.faulted,
            quiesced: st.quiesced,
        }
    }

    /// Replaces the queue's whole state (to start near a count's wrap). Wakes no one.
    #[cfg(test)]
    pub fn restore(&self, snap: &StampQueueSnapshot) {
        let mut st = imp::lock(&self.state);
        st.slots = snap.slots;
        st.write_count = snap.write_count;
        st.complete_count = snap.complete_count;
        st.faulted = snap.faulted;
        st.quiesced = snap.quiesced;
    }
}

/// A copy of the queue's state.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StampQueueSnapshot {
    /// The slots.
    pub slots: [PendingStamp; 128],
    /// Stamps pushed.
    pub write_count: u32,
    /// Stamps published.
    pub complete_count: u32,
    /// The fault latch.
    pub faulted: bool,
    /// The quiesced flag.
    pub quiesced: bool,
}

/// The FIFO's sink: its device, reached through the FIFO, which it holds weakly, and the stamp
/// index captured when the FIFO was made.
pub(crate) struct FifoSink<P: Platform> {
    pub(crate) fifo: Weak<Fifo<P>>,
    pub(crate) stamp_index: u32,
}

impl<P: Platform> StampSink for FifoSink<P> {
    /// The FIFO it loads is never the last reference here: the pump runs inside a method of the
    /// FIFO's own stamp queue, whose caller holds the FIFO.
    fn signal(&self, stamp_value: u32) {
        let Some(fifo) = self.fifo.upgrade() else {
            return;
        };
        if let Some(device) = fifo.device.upgrade() {
            device.signal_stamp(self.stamp_index, stamp_value);
        }
    }

    fn fault(&self, fault_offset: u32) {
        if let Some(fifo) = self.fifo.upgrade() {
            fifo.set_fault_offset(fault_offset);
            // The device signals the fault asynchronously, on its FIFO queue
            if let Some(device) = fifo.device.upgrade() {
                device.signal_fault_in_fifo(Arc::clone(&fifo));
            }
        }
    }

    fn head_not_faulted(&self) {
        log::warn!("resume: the stamp at the head of the queue is not faulted");
    }
}

#[cfg(all(test, not(loom)))]
#[allow(clippy::unwrap_used)] // A failure is the assertion
mod tests {
    use super::*;
    use crate::fifo::DeathKind;
    use crate::testkit::capture::catch_death;
    use crate::testkit::capture::expect_death;
    use crate::testkit::capture::expect_hang;
    use crate::testkit::clock::VirtualClock;
    use std::time::Duration;

    /// One call the queue made on its sink.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Ev {
        Signal(u32),
        Fault(u32),
        /// The resume found its head slot not faulted (the queue's one log line).
        HeadNotFaulted,
    }

    /// A recording sink. `queue`, once set, lets it check that the queue's mutex is held.
    #[derive(Default)]
    pub(super) struct Rec {
        pub(super) evs: Mutex<Vec<Ev>>,
        pub(super) locked_during_callbacks: Mutex<Vec<bool>>,
        pub(super) queue: Mutex<Option<std::sync::Weak<StampQueue>>>,
    }

    impl Rec {
        fn note_lock(&self) {
            let q = self
                .queue
                .lock()
                .as_ref()
                .and_then(std::sync::Weak::upgrade);
            if let Some(q) = q {
                self.locked_during_callbacks
                    .lock()
                    .push(q.state.is_locked());
            }
        }
    }

    impl StampSink for Arc<Rec> {
        fn signal(&self, stamp_value: u32) {
            self.note_lock();
            self.evs.lock().push(Ev::Signal(stamp_value));
        }
        fn fault(&self, fault_offset: u32) {
            self.note_lock();
            self.evs.lock().push(Ev::Fault(fault_offset));
        }
        fn head_not_faulted(&self) {
            self.evs.lock().push(Ev::HeadNotFaulted);
        }
    }

    pub(super) struct Q {
        pub(super) q: Arc<StampQueue>,
        pub(super) rec: Arc<Rec>,
        pub(super) last: Arc<AtomicU32>,
    }

    impl Q {
        pub(super) fn new() -> Q {
            let rec = Arc::new(Rec::default());
            let last = Arc::new(AtomicU32::new(0xdead));
            let q = Arc::new(StampQueue::new(last.clone(), Box::new(rec.clone())));
            *rec.queue.lock() = Some(Arc::downgrade(&q));
            Q { q, rec, last }
        }
        /// The sink calls.
        pub(super) fn calls(&self) -> Vec<Ev> {
            self.rec.evs.lock().clone()
        }
        pub(super) fn all(&self) -> Vec<Ev> {
            self.rec.evs.lock().clone()
        }
        pub(super) fn clear(&self) {
            self.rec.evs.lock().clear();
        }
        pub(super) fn snap(&self) -> StampQueueSnapshot {
            self.q.snapshot()
        }
        fn pump(&self) {
            let mut st = self.q.state.lock();
            self.q.pump(&mut st);
        }
    }

    const fn slot(stamp_value: u32, command_id: u32, flags: u32) -> PendingStamp {
        PendingStamp {
            stamp_value,
            command_id,
            flags,
        }
    }

    mod pending_stamp_bits {
        use super::*;

        #[test]
        fn bit_fields_share_the_flags_word() {
            let mut p = PendingStamp::default();
            p.set_signaled(true);
            assert_eq!(p.flags, 1);
            p.set_faulted(true);
            assert_eq!(p.flags, 3);
            p.set_offset(0x1234);
            assert_eq!(p.flags, 3 | (0x1234 << 2));
            assert_eq!(p.offset(), 0x1234);
            p.set_signaled(false);
            assert!(!p.signaled() && p.faulted());
            assert_eq!(p.offset(), 0x1234);
            p.set_faulted(false);
            assert_eq!(p.flags, 0x1234 << 2);
            // The field is 30 bits wide.
            p.set_offset(0x7fff_ffff);
            assert_eq!(p.offset(), 0x3fff_ffff);
            assert_eq!(p.flags & 3, 0);
        }
    }

    mod new_queue {
        use super::*;

        #[test]
        fn starts_zeroed_and_drop_publishes_nothing() {
            let t = Q::new();
            let s = t.snap();
            assert_eq!(s.slots, [PendingStamp::default(); 128]);
            assert_eq!((s.write_count, s.complete_count), (0, 0));
            assert!(!s.faulted && !s.quiesced);
            assert_eq!(t.last.load(Ordering::Relaxed), 0xdead);
            t.q.push_stamp(1, 2);
            let Q { q, rec, .. } = t;
            drop(q);
            // The destructor releases the blocks: the sink is gone, nothing was published.
            assert_eq!(Arc::strong_count(&rec), 1);
            assert!(rec.evs.lock().is_empty());
        }
    }

    mod push {
        use super::*;

        #[test]
        fn fills_the_next_slot_and_stores_last_pending() {
            let t = Q::new();
            t.q.push_stamp(0x10, 7);
            t.q.push_stamp(0x11, 0xffff);
            let s = t.snap();
            assert_eq!(s.slots[0], slot(0x10, 7, 0));
            assert_eq!(s.slots[1], slot(0x11, 0xffff, 0));
            assert_eq!((s.write_count, s.complete_count), (2, 0));
            assert_eq!(t.last.load(Ordering::Relaxed), 0x11);
            // Nothing is signalled.
            assert_eq!(t.all(), vec![]);
        }

        /// A stale slot's flags are cleared
        #[test]
        fn overwrites_a_stale_slot_whole() {
            let t = Q::new();
            let mut s = t.snap();
            s.slots[5] = slot(9, 9, 0xffff_ffff);
            s.write_count = 5;
            s.complete_count = 5;
            t.q.restore(&s);
            t.q.push_stamp(3, 4);
            assert_eq!(t.snap().slots[5], slot(3, 4, 0));
        }

        /// The counts wrap; the slot is `% 128`
        #[test]
        fn write_count_wraps() {
            let t = Q::new();
            let mut s = t.snap();
            s.write_count = u32::MAX;
            s.complete_count = u32::MAX;
            t.q.restore(&s);
            t.q.push_stamp(1, 1);
            t.q.push_stamp(2, 2);
            let s = t.snap();
            assert_eq!(s.write_count, 1);
            assert_eq!(s.slots[127], slot(1, 1, 0));
            assert_eq!(s.slots[0], slot(2, 2, 0));
            // The window crosses the wrap: signalling both publishes the last one.
            t.q.signal_stamp(1);
            t.q.signal_stamp(2);
            assert_eq!(t.calls(), vec![Ev::Signal(1), Ev::Signal(2)]);
            assert_eq!(t.snap().complete_count, 1);
        }

        #[test]
        fn pushes_while_quiesced() {
            let t = Q::new();
            t.q.quiesce();
            t.q.push_stamp(4, 4);
            assert_eq!(t.snap().write_count, 1);
        }

        /// Blocks while 128 are pending
        #[test]
        fn blocks_at_128_pending() {
            let t = Q::new();
            for v in 0..128 {
                t.q.push_stamp(v, 0);
            }
            let q = t.q.clone();
            expect_hang(&VirtualClock::new(), Duration::from_secs(3600), move || {
                q.push_stamp(128, 0)
            });
        }

        /// A pump that completes one wakes the pusher
        #[test]
        #[allow(clippy::disallowed_methods)] // a test thread plays the FIFO thread
        fn wakes_when_the_head_completes() {
            let t = Q::new();
            for v in 0..128 {
                t.q.push_stamp(v, 0);
            }
            let q = t.q.clone();
            let pusher = std::thread::spawn(move || q.push_stamp(500, 1));
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(t.snap().write_count, 128);
            t.q.signal_stamp(0);
            pusher.join().unwrap();
            let s = t.snap();
            assert_eq!((s.write_count, s.complete_count), (129, 1));
            assert_eq!(s.slots[0], slot(500, 1, 0));
            assert_eq!(t.last.load(Ordering::Relaxed), 500);
        }
    }

    mod signal {
        use super::*;

        #[test]
        fn dies_when_nothing_is_pending() {
            expect_death(DeathKind::SignalNothingPending.site(), || {
                Q::new().q.signal_stamp(1)
            });
        }

        /// The head: marks, pumps, publishes
        #[test]
        fn head_signal_publishes() {
            let t = Q::new();
            t.q.push_stamp(0x20, 1);
            t.q.push_stamp(0x21, 2);
            t.clear();
            t.q.signal_stamp(0x20);
            assert_eq!(t.all(), vec![Ev::Signal(0x20)]);
            let s = t.snap();
            assert_eq!(s.complete_count, 1);
            // The completed slot keeps its flags.
            assert_eq!(s.slots[0], slot(0x20, 1, 1));
            // The callback ran with the queue's mutex held.
            assert_eq!(*t.rec.locked_during_callbacks.lock(), vec![true]);
        }

        /// Behind an unsignalled head: marks only
        #[test]
        fn out_of_order_waits_for_the_head() {
            let t = Q::new();
            for v in [1, 2, 3] {
                t.q.push_stamp(v, 0);
            }
            t.q.signal_stamp(3);
            t.q.signal_stamp(2);
            assert_eq!(t.calls(), vec![]);
            assert_eq!(t.snap().slots[1].flags, 1);
            assert_eq!(t.snap().slots[2].flags, 1);
            // The head completes: one signal, the last contiguous value.
            t.q.signal_stamp(1);
            assert_eq!(t.calls(), vec![Ev::Signal(3)]);
            assert_eq!(t.snap().complete_count, 3);
        }

        /// A value not pending marks nothing
        #[test]
        fn unknown_value_marks_nothing() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            let before = t.snap();
            t.q.signal_stamp(99);
            assert_eq!(t.snap(), before);
            assert_eq!(t.calls(), vec![]);
        }

        /// Every slot signalled: pumps anyway
        #[test]
        fn unknown_value_over_signalled_window_pumps() {
            let t = Q::new();
            let mut s = t.snap();
            s.slots[0] = slot(1, 0, 1);
            s.write_count = 1;
            t.q.restore(&s);
            t.q.signal_stamp(99);
            assert_eq!(t.calls(), vec![Ev::Signal(1)]);
            assert_eq!(t.snap().complete_count, 1);
        }

        /// A faulted head clears atHead
        #[test]
        fn behind_a_faulted_head_does_not_pump() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.push_stamp(2, 0);
            t.q.fault_stamp(1, 0x40);
            t.clear();
            t.q.signal_stamp(2);
            assert_eq!(t.calls(), vec![]);
            assert_eq!(t.snap().slots[1].flags, 1);
        }

        /// A duplicate dies
        #[test]
        fn duplicate_signal_dies() {
            expect_death(DeathKind::SignalTwice.site(), || {
                let t = Q::new();
                for v in [1, 5, 5] {
                    t.q.push_stamp(v, 0);
                }
                t.q.signal_stamp(5);
                // The first 5 is signalled but not complete: the scan finds it again.
                t.q.signal_stamp(5);
            });
        }

        /// Signalling a faulted stamp dies
        #[test]
        fn signal_of_faulted_dies() {
            expect_death(DeathKind::SignalFaulted.site(), || {
                let t = Q::new();
                t.q.push_stamp(1, 0);
                t.q.push_stamp(5, 0);
                t.q.fault_stamp(5, 8);
                t.q.signal_stamp(5);
            });
        }

        /// Two pending with one value: the first
        #[test]
        fn same_value_twice_completes_in_order() {
            let t = Q::new();
            t.q.push_stamp(5, 0);
            t.q.push_stamp(5, 1);
            t.q.signal_stamp(5);
            assert_eq!(t.snap().complete_count, 1);
            t.q.signal_stamp(5);
            assert_eq!(t.calls(), vec![Ev::Signal(5), Ev::Signal(5)]);
            assert_eq!(t.snap().complete_count, 2);
        }

        /// A late second completion
        #[test]
        fn late_second_completion_is_swallowed_or_dies() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.push_stamp(2, 0);
            t.q.signal_stamp(1);
            // 1 is complete; its second completion finds 2 unsignalled and does nothing.
            let before = t.snap();
            t.q.signal_stamp(1);
            assert_eq!(t.snap(), before);
            t.q.signal_stamp(2);
            // Nothing is pending now: the next one dies.
            let q = t.q.clone();
            assert_eq!(
                catch_death(move || q.signal_stamp(2)),
                Err(DeathKind::SignalNothingPending.site())
            );
        }
    }

    mod fault {
        use super::*;

        #[test]
        fn dies_when_nothing_is_pending() {
            expect_death(DeathKind::FaultNothingPending.site(), || {
                Q::new().q.fault_stamp(1, 0)
            });
        }

        /// Offset 2^31 dies
        #[test]
        fn dies_at_offset_2_31() {
            expect_death(DeathKind::FaultOffsetRange.site(), || {
                let t = Q::new();
                t.q.push_stamp(1, 0);
                t.q.fault_stamp(1, 0x8000_0000);
            });
        }

        /// The pending check comes first
        #[test]
        fn empty_check_precedes_offset_check() {
            expect_death(DeathKind::FaultNothingPending.site(), || {
                Q::new().q.fault_stamp(1, 0x8000_0000)
            });
        }

        /// 30-bit storage
        #[test]
        fn offset_is_stored_in_30_bits() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.fault_stamp(1, 0x7fff_fffc);
            assert_eq!(t.calls(), vec![Ev::Fault(0x3fff_fffc)]);
            assert_eq!(t.snap().slots[0].flags, 2 | (0x3fff_fffc << 2));
        }

        /// The head: marks, pumps, reports once
        #[test]
        fn head_fault_reports_and_latches() {
            let t = Q::new();
            t.q.push_stamp(1, 3);
            t.q.push_stamp(2, 4);
            t.clear();
            t.q.fault_stamp(1, 0x80);
            assert_eq!(t.all(), vec![Ev::Fault(0x80)]);
            let s = t.snap();
            assert!(s.faulted);
            assert_eq!(s.complete_count, 0);
            assert_eq!(s.slots[0], slot(1, 3, 2 | (0x80 << 2)));
            assert_eq!(*t.rec.locked_during_callbacks.lock(), vec![true]);
        }

        /// Behind an unsignalled head: marks only
        #[test]
        fn behind_the_head_marks_only() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.push_stamp(2, 0);
            t.q.fault_stamp(2, 0x10);
            assert_eq!(t.calls(), vec![]);
            assert!(!t.snap().faulted);
            // The head completes: it is published, then the faulted slot behind it is reported.
            t.clear();
            t.q.signal_stamp(1);
            assert_eq!(t.all(), vec![Ev::Signal(1), Ev::Fault(0x10)]);
            assert_eq!(t.snap().complete_count, 1);
        }

        /// A value not pending marks nothing
        #[test]
        fn unknown_value_marks_nothing() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            let before = t.snap();
            t.q.fault_stamp(9, 4);
            assert_eq!(t.snap(), before);
            assert_eq!(t.calls(), vec![]);
        }

        /// Faulting a signalled stamp
        #[test]
        fn fault_of_signalled_dies() {
            expect_death(DeathKind::FaultSignalled.site(), || {
                let t = Q::new();
                t.q.push_stamp(1, 0);
                t.q.push_stamp(5, 0);
                t.q.signal_stamp(5);
                t.q.fault_stamp(5, 0);
            });
        }

        /// Faulting a faulted stamp dies
        #[test]
        fn fault_of_faulted_dies() {
            expect_death(DeathKind::FaultTwice.site(), || {
                let t = Q::new();
                t.q.push_stamp(1, 0);
                t.q.push_stamp(5, 0);
                t.q.fault_stamp(5, 0);
                t.q.fault_stamp(5, 0);
            });
        }
    }

    mod resume_from_fault {
        use super::*;

        /// A faulted head is signalled
        #[test]
        fn resumes_a_faulted_head() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.push_stamp(2, 0);
            t.q.signal_stamp(2);
            t.q.fault_stamp(1, 0x20);
            t.clear();
            t.q.resume_from_fault();
            assert_eq!(t.calls(), vec![Ev::Signal(2)]);
            let s = t.snap();
            assert!(!s.faulted);
            assert_eq!(s.complete_count, 2);
            // faulted cleared, signalled set, the offset kept.
            assert_eq!(s.slots[0].flags, 1 | (0x20 << 2));
        }

        /// A head not faulted
        #[test]
        fn logs_when_the_head_is_not_faulted() {
            let t = Q::new();
            let mut s = t.snap();
            s.faulted = true;
            s.slots[0] = slot(1, 0, 1);
            s.write_count = 1;
            t.q.restore(&s);
            t.q.resume_from_fault();
            // The latch stays set; the pump still runs and publishes the signalled head.
            assert_eq!(t.calls(), vec![Ev::HeadNotFaulted, Ev::Signal(1)]);
            assert!(t.snap().faulted);
        }

        /// The stale head slot
        #[test]
        fn resumes_a_stale_faulted_slot() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.fault_stamp(1, 0x44);
            // The quiesce publishes the faulted head and keeps the latch.
            t.q.quiesce();
            let s = t.snap();
            assert_eq!((s.write_count, s.complete_count), (1, 1));
            assert!(s.faulted);
            t.clear();
            // Nothing pending, but slot 1 % 128 is read: never used, so not faulted.
            t.q.resume_from_fault();
            assert_eq!(t.calls(), vec![Ev::HeadNotFaulted]);
            // Make slot 1 a stale faulted slot: it is "resumed" and the latch clears.
            let mut s = t.snap();
            s.slots[1] = slot(7, 7, 2 | (0x8 << 2));
            t.q.restore(&s);
            t.clear();
            t.q.resume_from_fault();
            assert_eq!(t.calls(), vec![]);
            let s = t.snap();
            assert!(!s.faulted);
            assert_eq!(s.slots[1].flags, 1 | (0x8 << 2));
            assert_eq!(s.complete_count, 1);
        }
    }

    mod pump {
        use super::*;

        /// Nothing complete: no signal
        #[test]
        fn nothing_complete_publishes_nothing() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.clear();
            t.pump();
            assert_eq!(t.all(), vec![]);
        }

        /// A reported fault is not reported again
        #[test]
        fn fault_is_reported_once_per_latch() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.fault_stamp(1, 4);
            t.pump();
            t.pump();
            assert_eq!(t.calls(), vec![Ev::Fault(4)]);
        }

        /// Quiesced, faulted slots complete
        #[test]
        fn quiesced_consumes_faulted_slots() {
            let t = Q::new();
            for v in [1, 2, 3, 4] {
                t.q.push_stamp(v, 0);
            }
            t.q.fault_stamp(2, 8);
            t.q.signal_stamp(3);
            t.q.signal_stamp(1);
            assert_eq!(t.calls(), vec![Ev::Signal(1), Ev::Fault(8)]);
            t.clear();
            t.q.quiesce();
            // 2 (faulted) and 3 are consumed; 4 stops the scan. No fault callback.
            assert_eq!(t.calls(), vec![Ev::Signal(3)]);
            let s = t.snap();
            assert_eq!(s.complete_count, 3);
            assert!(s.faulted && s.quiesced);
        }

        /// Unquiesced, a latched fault still stops the scan
        #[test]
        fn latched_fault_stops_without_report() {
            let t = Q::new();
            let mut s = t.snap();
            s.faulted = true;
            s.slots[0] = slot(1, 0, 1);
            s.slots[1] = slot(2, 0, 2 | (5 << 2));
            s.write_count = 2;
            t.q.restore(&s);
            t.pump();
            assert_eq!(t.calls(), vec![Ev::Signal(1)]);
            assert_eq!(t.snap().complete_count, 1);
        }
    }

    mod quiesce {
        use super::*;

        #[test]
        fn sets_the_flag_and_pumps() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.quiesce();
            assert!(t.snap().quiesced);
            assert_eq!(t.calls(), vec![]);
            // A faulted head, then a quiesce: published, latch kept.
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.push_stamp(2, 0);
            t.q.fault_stamp(1, 12);
            t.clear();
            t.q.quiesce();
            assert_eq!(t.calls(), vec![Ev::Signal(1)]);
            assert!(t.snap().faulted);
        }
    }

    mod start_clears_quiesced {
        use super::*;

        /// `_pending._quiesced = NO` under the lock
        #[test]
        fn start_clears_quiesced_so_a_later_head_fault_is_reported() {
            let t = Q::new();
            t.q.quiesce();
            t.q.clear_quiesced();
            t.q.push_stamp(1, 0);
            t.q.push_stamp(2, 0);
            t.clear();
            t.q.fault_stamp(1, 0x40);
            assert_eq!(t.calls(), vec![Ev::Fault(0x40)]);
            let s = t.snap();
            assert!(!s.quiesced && s.faulted);
            assert_eq!(s.complete_count, 0);
        }

        /// No pump, no wake, the fault latch kept
        #[test]
        fn clearing_runs_no_pump_and_keeps_the_latch() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.push_stamp(2, 0);
            t.q.fault_stamp(1, 12);
            t.q.quiesce();
            t.q.push_stamp(3, 0);
            t.q.signal_stamp(3);
            t.clear();
            let before = t.snap();
            t.q.clear_quiesced();
            assert_eq!(t.all(), vec![]);
            let after = t.snap();
            assert!(!after.quiesced && after.faulted);
            assert_eq!(after.slots, before.slots);
            assert_eq!(
                (after.write_count, after.complete_count),
                (before.write_count, before.complete_count)
            );
        }
    }

    mod drain {
        use super::*;

        #[test]
        fn dies_unless_quiesced() {
            expect_death(DeathKind::QueueDrainUnquiesced.site(), || {
                Q::new().q.drain()
            });
        }

        /// Everything complete or faulted: returns
        #[test]
        fn returns_when_everything_completes() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.push_stamp(2, 0);
            t.q.fault_stamp(1, 0);
            t.q.signal_stamp(2);
            let mut s = t.snap();
            s.quiesced = true;
            t.q.restore(&s);
            t.clear();
            // drain's own pump publishes what quiesce would have.
            t.q.drain();
            assert_eq!(t.calls(), vec![Ev::Signal(2)]);
            assert_eq!(t.snap().complete_count, 2);
        }

        /// A stamp that never completes
        #[test]
        fn hangs_on_a_pending_stamp() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.quiesce();
            let q = t.q.clone();
            expect_hang(&VirtualClock::new(), Duration::from_secs(3600), move || {
                q.drain()
            });
        }

        /// A later completion wakes it
        #[test]
        #[allow(clippy::disallowed_methods)] // a test thread plays the lifecycle caller
        fn wakes_when_the_last_completes() {
            let t = Q::new();
            t.q.push_stamp(1, 0);
            t.q.quiesce();
            let q = t.q.clone();
            let drainer = std::thread::spawn(move || q.drain());
            std::thread::sleep(Duration::from_millis(50));
            assert!(!drainer.is_finished());
            t.q.signal_stamp(1);
            drainer.join().unwrap();
            assert_eq!(t.snap().complete_count, 1);
        }
    }

    mod describe_pending {
        use super::*;

        #[test]
        fn empty() {
            assert_eq!(Q::new().q.describe_pending(), "");
        }

        #[test]
        fn lists_the_window() {
            let t = Q::new();
            t.q.push_stamp(0xab, 0x3d);
            t.q.push_stamp(0, 0);
            t.q.push_stamp(0xcd, 0x3e);
            t.q.signal_stamp(0xcd);
            t.q.fault_stamp(0, 4);
            assert_eq!(
                t.q.describe_pending(),
                "\tCMD=0x3d signalStamp=0xab signaled=0 faulted=0\n\
                 \tCMD=0x0 signalStamp=0x0 signaled=0 faulted=1\n\
                 \tCMD=0x3e signalStamp=0xcd signaled=1 faulted=0\n"
            );
        }

        /// 10 shown, then the rest counted
        #[test]
        fn shows_ten_then_counts() {
            let t = Q::new();
            let mut s = t.snap();
            s.write_count = 3;
            s.complete_count = 3;
            t.q.restore(&s);
            for v in 0..12 {
                t.q.push_stamp(v, 1);
            }
            let d = t.q.describe_pending();
            assert_eq!(d.lines().count(), 11);
            assert!(d.starts_with("\tCMD=0x1 signalStamp=0x0 signaled=0 faulted=0\n"));
            assert!(d.ends_with("signalStamp=0x9 signaled=0 faulted=0\n\tAnd 2 more commands"));
            // Exactly 10: no count line.
            let t = Q::new();
            for v in 0..10 {
                t.q.push_stamp(v, 1);
            }
            assert_eq!(t.q.describe_pending().lines().count(), 10);
        }
    }
}

/// The `loom` model check (architecture 13.2): `RUSTFLAGS="--cfg loom" cargo test -p
/// reims-vgpu-core --lib fifo::stamps::loom_tests --release`.
#[cfg(all(test, loom))]
#[allow(clippy::unwrap_used, clippy::panic)] // A failure is the assertion
mod loom_tests {
    use super::*;

    /// A sink that counts the published signals and faults (the model's only observers).
    struct Count(loom::sync::Arc<loom::sync::atomic::AtomicU32>);

    impl StampSink for Count {
        fn signal(&self, _stamp_value: u32) {
            self.0.fetch_add(1, loom::sync::atomic::Ordering::SeqCst);
        }
        fn fault(&self, _fault_offset: u32) {
            self.0
                .fetch_add(0x100, loom::sync::atomic::Ordering::SeqCst);
        }
        fn head_not_faulted(&self) {}
    }

    fn full_queue() -> (
        loom::sync::Arc<StampQueue>,
        loom::sync::Arc<loom::sync::atomic::AtomicU32>,
    ) {
        let n = loom::sync::Arc::new(loom::sync::atomic::AtomicU32::new(0));
        let q = StampQueue::new(Arc::new(AtomicU32::new(0)), Box::new(Count(n.clone())));
        {
            let mut st = imp::lock(&q.state);
            for v in 0..STAMP_QUEUE_CAPACITY {
                let slot = st.slot(v);
                slot.stamp_value = v;
            }
            st.write_count = STAMP_QUEUE_CAPACITY;
        }
        (loom::sync::Arc::new(q), n)
    }

    /// Two pushers block on a full queue; one
    /// signal completes two slots and wakes one of them. A pusher still blocked after that stays so
    /// until a later pump wakes it: no interleaving loses the wakeup (loom reports the deadlock),
    /// and none lets a pusher write over a pending slot.
    #[test]
    fn one_pump_wakes_one_blocked_pusher() {
        loom::model(|| {
            let (q, published) = full_queue();
            let mut hs = Vec::new();
            for v in [500, 501] {
                let q = q.clone();
                hs.push(loom::thread::spawn(move || q.push_stamp(v, 1)));
            }
            // Both slots at the head complete in one pump.
            q.signal_stamp(1);
            q.signal_stamp(0);
            {
                let st = imp::lock(&q.state);
                assert!(st.pending_count() <= STAMP_QUEUE_CAPACITY);
                assert_eq!(st.complete_count, 2);
            }
            // A later pump (the next completion) wakes whichever pusher is still waiting.
            q.signal_stamp(2);
            for h in hs {
                h.join().unwrap();
            }
            let st = imp::lock(&q.state);
            assert_eq!((st.write_count, st.complete_count), (130, 3));
            assert_eq!(published.load(loom::sync::atomic::Ordering::SeqCst), 2);
        });
    }

    /// A drain blocked on an unsignalled head
    /// wakes when a signal completes the queue.
    #[test]
    fn drain_wakes_when_the_queue_empties() {
        loom::model(|| {
            let n = loom::sync::Arc::new(loom::sync::atomic::AtomicU32::new(0));
            let q = loom::sync::Arc::new(StampQueue::new(
                Arc::new(AtomicU32::new(0)),
                Box::new(Count(n.clone())),
            ));
            q.push_stamp(1, 0);
            q.quiesce();
            let q2 = q.clone();
            let h = loom::thread::spawn(move || q2.drain());
            q.signal_stamp(1);
            h.join().unwrap();
            let st = imp::lock(&q.state);
            assert_eq!(st.write_count, st.complete_count);
            assert_eq!(n.load(loom::sync::atomic::Ordering::SeqCst), 1);
        });
    }
}
