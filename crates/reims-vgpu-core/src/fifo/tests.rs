//! The FIFO's unit tests: init, stamps, registers, cursors, the ring reads, mapping, the
//! lifecycle, the thread's loop, and the stamp queue's sink.

#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::testkit::*;
use super::*;
use crate::fifo::DeathKind;
use crate::fifo::ring::Ring;
use crate::fifo::stamps::{FifoSink, StampSink};
use crate::fifo::wire::StampWait;
use crate::rt::thread::FIFO_THREAD_STACK;
use crate::testkit::Step;
use crate::testkit::capture::catch_death;

/// The FIFO area's lines `logs` took since its last take, at warn or above, in order.
fn fifo_lines(logs: &crate::testkit::logs::Capture) -> Vec<String> {
    logs.take()
        .into_iter()
        .filter(|(level, target, _)| {
            *level <= log::Level::Warn
                && target.starts_with(concat!(env!("CARGO_CRATE_NAME"), "::fifo"))
        })
        .map(|(_, _, message)| message)
        .collect()
}

/// Stops a started FIFO the way the device does: quiesce, then drain.
fn shut(f: &Arc<Fifo<P>>) {
    f.quiesce();
    f.drain();
}

/// The doorbells [`ring_in_the_gap`] rings, each once, on the thread of its name.
type Doorbells = Vec<(&'static str, Box<dyn FnOnce() + Send>)>;
static DOORBELLS: crate::sync::Mutex<Doorbells> = crate::sync::Mutex::new(Vec::new());

/// The before-block hook of the gap tests: rings the current thread's doorbell, as the guest's
/// can come while a FIFO thread submits its held-back work with the ring's lock free, and
/// submits nothing.
fn ring_in_the_gap() -> bool {
    let name = std::thread::current().name().map(str::to_owned);
    let doorbell = {
        let mut doorbells = DOORBELLS.lock();
        let at = doorbells
            .iter()
            .position(|(n, _)| Some(*n) == name.as_deref());
        at.map(|at| doorbells.remove(at).1)
    };
    if let Some(ring) = doorbell {
        ring();
    }
    false
}

/// `n` bytes of `f`'s ring, read on a thread named `name` whose first wait for bytes has
/// `doorbell` rung in its gap; `None` where the thread still waits once the helper stops
/// waiting for it.
fn read_past_a_doorbell_in_the_gap(
    f: &Arc<Fifo<P>>,
    name: &'static str,
    n: usize,
    doorbell: impl FnOnce() + Send + 'static,
) -> Option<Vec<u8>> {
    crate::rt::thread::set_before_block(ring_in_the_gap);
    DOORBELLS.lock().push((name, Box::new(doorbell)));
    let g = f.clone();
    let t = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut b = vec![0u8; n];
            g.read_bytes(&mut b).then_some(b)
        })
        .unwrap();
    let finished = wait_for(|| t.is_finished());
    assert!(
        !DOORBELLS.lock().iter().any(|(n, _)| *n == name),
        "the thread never waited"
    );
    finished.then(|| t.join().unwrap()).flatten()
}

mod root_init {
    use super::*;

    #[test]
    fn fields_and_event() {
        let kit = Kit::new();
        let (f, ram) = kit.root(0x1000, 0x400);
        assert!(f.is_root_fifo());
        assert_eq!(f.stamp_index(), 0);
        assert_eq!(f.ring_length.load(Ordering::Relaxed), 0xc00);
        assert_eq!(f.fault_offset(), u32::MAX);
        assert_eq!(f.root_written(), Some(0));
        assert_eq!(f.root_read(), Some(0));
        assert_eq!(f.root_start_offset(), Some(0x400));
        let Ring::Root(r) = &f.ring else { panic!() };
        assert_eq!(r.base.addr(), ram.addr().wrapping_add(0x400));
        // The shared event from the device's Metal device, one per FIFO.
        assert!(f.event.is_some());
        assert!(
            kit.platform
                .call_log()
                .contains_in_order(&["newSharedEvent"])
        );
        assert_eq!(f.event_value.load(Ordering::SeqCst), 0);
        assert!(!f.is_quiesced());
        assert!(!f.running.load(Ordering::Relaxed));
    }

    /// Length <= startOffset is nil
    #[test]
    fn start_not_below_length_is_nil() {
        let kit = Kit::new();
        let ram = crate::testkit::guest_ram::RawRegion::new(0x4000).unwrap();
        let base = GuestPtr::new(ram.addr());
        for (len, start) in [(0x4000, 0x4000), (0x400, 0x800), (0, 0)] {
            assert!(
                Fifo::try_new_root_on(
                    &kit.platform,
                    kit.weak(),
                    len,
                    base,
                    start,
                    kit.last_pending.clone()
                )
                .is_none()
            );
            let f = kit.root_at(len, base, start);
            assert_eq!(f.ring_length.load(Ordering::Relaxed), 0);
            assert_eq!(f.root_start_offset(), Some(start));
        }
        assert!(
            Fifo::try_new_root_on(
                &kit.platform,
                kit.weak(),
                0x4000,
                base,
                0x3fff,
                kit.last_pending.clone()
            )
            .is_some()
        );
    }

    /// A nil device gives a nil event
    #[test]
    fn nil_device_nil_event() {
        let kit = Kit::new();
        let weak = kit.weak();
        let ram = crate::testkit::guest_ram::RawRegion::new(0x4000).unwrap();
        drop(kit.host);
        drop(kit.td);
        let f = Fifo::init_root(
            &kit.platform,
            weak,
            0x4000,
            GuestPtr::new(ram.addr()),
            0x400,
            kit.last_pending.clone(),
        )
        .0;
        assert!(f.event.is_none());
    }
}

mod child_init {
    use super::*;

    /// A create writes the node
    #[test]
    fn create_writes_node() {
        let kit = Kit::new();
        kit.features(|f| f.max_fifo_count = 32);
        let m = ChildMem::new(&kit, 1, 5);
        m.node.write_u32(4, 0x55);
        m.node.write_u32(8, 0x66);
        let f = kit.child(m.node_ptr(), false).unwrap();
        assert!(!f.is_root_fifo());
        assert_eq!(f.stamp_index(), 5);
        assert_eq!(f.fault_offset(), u32::MAX);
        assert_eq!(m.node.read_u32(4), Some(0));
        assert_eq!(m.node.read_u32(8), Some(u32::MAX));
        assert_eq!(f.root_written(), None);
        assert_eq!(f.root_start_offset(), None);
        assert_eq!(f.root_read(), None);
    }

    /// A restore reads the node and leaves it
    #[test]
    fn restore_reads_node() {
        let kit = Kit::new();
        kit.features(|f| f.max_fifo_count = 32);
        let m = ChildMem::new(&kit, 1, 2);
        m.node.write_u32(0, 0x90);
        m.node.write_u32(4, 0x80);
        m.node.write_u32(8, 0x44);
        let f = kit.child(m.node_ptr(), true).unwrap();
        let Ring::Child(c) = &f.ring else { panic!() };
        assert_eq!(c.read_count.load(Ordering::Relaxed), 0x80);
        assert_eq!(c.last_read_count.load(Ordering::Relaxed), 0x80);
        assert_eq!(c.cache_written.load(Ordering::Relaxed), 0x90);
        assert_eq!(f.fault_offset(), 0x44);
        assert_eq!(m.node.read_u32(4), Some(0x80));
        assert_eq!(m.node.read_u32(8), Some(0x44));
    }

    /// A refused stamp index has already written the node
    #[test]
    fn refused_stamp_index_keeps_node_writes() {
        let kit = Kit::new();
        kit.features(|f| f.max_fifo_count = 32);
        let m = ChildMem::new(&kit, 1, 32);
        m.node.write_u32(4, 0x55);
        m.node.write_u32(8, 0x66);
        assert!(kit.child(m.node_ptr(), false).is_none());
        assert_eq!(m.node.read_u32(4), Some(0));
        assert_eq!(m.node.read_u32(8), Some(u32::MAX));
        // 31 < 32 passes.
        m.node.write(0xc, &31u16.to_le_bytes());
        assert!(kit.child(m.node_ptr(), false).is_some());
    }

    /// The stamp index is read once, at init
    #[test]
    fn stamp_index_captured_at_init() {
        let kit = Kit::new();
        kit.features(|f| f.max_fifo_count = 32);
        let m = ChildMem::new(&kit, 1, 0);
        let f = kit.child(m.node_ptr(), false).unwrap();
        m.node.write(0xc, &7u16.to_le_bytes());
        assert_eq!(f.stamp_index(), 0);
        f.push_stamp(1, 30);
        f.signal_stamp_value(1);
        assert_eq!(kit.signals(), vec![(0, 1)]);
    }

    /// A gone device dies at the features read
    #[test]
    fn gone_device_dies() {
        let kit = Kit::new();
        let weak = kit.weak();
        let m = ChildMem::new(&kit, 1, 0);
        drop(kit.host);
        drop(kit.td);
        let lp = kit.last_pending.clone();
        let platform = kit.platform.clone();
        let r = catch_death(move || {
            let _ = Fifo::new_child_on(&platform, weak, m.node_ptr(), 1, false, lp);
        });
        assert_eq!(r, Err(DeathKind::DeviceGone.site()));
    }
}

mod stamps {
    use super::*;

    #[test]
    fn push_signal_describe() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        f.push_stamp(0x11, 0xffff);
        assert_eq!(kit.last_pending.load(Ordering::Relaxed), 0x11);
        let d = f.describe_pending();
        assert!(d.contains("CMD=0xffff signalStamp=0x11"), "{d}");
        f.signal_stamp_value(0x11);
        assert_eq!(kit.signals(), vec![(0, 0x11)]);
        assert_eq!(f.describe_pending(), "");
    }

    /// A gone device drops the publication
    #[test]
    fn gone_device_publishes_nothing() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        let Kit { host, td, .. } = kit;
        let stamps = td.stamp_ram.clone().unwrap();
        let fake_host = td.host.clone();
        drop(host);
        drop(td);
        let interrupts = fake_host.interrupts().len();
        f.push_stamp(1, 30);
        f.signal_stamp_value(1);
        f.push_stamp(2, 30);
        f.fault_at_offset(8, 2);
        assert_eq!(stamps.read_u32(0), Some(0));
        assert_eq!(fake_host.interrupts().len(), interrupts);
        // The fault offset is still set (`[fifo setFaultOffset:]` runs on the live FIFO).
        assert_eq!(f.fault_offset(), 8);
    }

    /// Child: the node's fault offset is cleared too
    #[test]
    fn child_fault_offset_reaches_node() {
        let kit = Kit::new();
        kit.features(|f| f.max_fifo_count = 32);
        let m = ChildMem::new(&kit, 1, 4);
        let f = kit.child(m.node_ptr(), false).unwrap();
        f.push_stamp(9, 30);
        f.command_offset.store(0x30, Ordering::Relaxed);
        f.fault_at_stamp_value(9);
        assert_eq!(m.node.read_u32(8), Some(0x30));
        assert_eq!(f.fault_offset(), 0x30);
        f.resume();
        assert_eq!(m.node.read_u32(8), Some(u32::MAX));
        assert_eq!(kit.signals(), vec![(4, 9)]);
        // The FIFO's own field, not the node, answers.
        m.node.write_u32(8, 5);
        assert_eq!(f.fault_offset(), u32::MAX);
    }
}

mod root_registers {
    use super::*;

    #[test]
    fn registers() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        f.set_root_written(0xdead_beef);
        assert_eq!(f.root_written(), Some(0xdead_beef));
        f.set_root_read(0xc05);
        assert_eq!(f.root_read(), Some(0xc05));
        let Ring::Root(r) = &f.ring else { panic!() };
        assert_eq!(r.read_offset.load(Ordering::Relaxed), 5);
        assert_eq!(r.last_read_offset.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn accessors() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        f.set_fault_offset(12);
        assert_eq!(f.fault_offset(), 12);
        assert!(f.is_root_fifo());
        assert_eq!(f.stamp_index(), ROOT_STAMP_INDEX);
        assert!(!f.is_quiesced());
        f.quiesce();
        assert!(f.is_quiesced());
        // Setters of the other kind do nothing.
        kit.features(|x| x.max_fifo_count = 8);
        let m = ChildMem::new(&kit, 1, 3);
        let c = kit.child(m.node_ptr(), false).unwrap();
        assert!(!c.is_root_fifo());
        assert_eq!(c.stamp_index(), 3);
        c.set_root_written(5);
        c.set_root_read(5);
        assert_eq!(c.root_written(), None);
    }
}

mod cursors {
    use super::*;

    #[test]
    fn root_cursors() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        let Ring::Root(r) = &f.ring else { panic!() };
        r.read_offset.store(0x10, Ordering::Relaxed);
        f.latch_offset();
        assert_eq!(f.command_offset.load(Ordering::Relaxed), 0x10);
        r.read.store(u32::MAX - 1, Ordering::SeqCst);
        r.pending_advance.store(4, Ordering::Relaxed);
        f.advance();
        // u32 wrap.
        assert_eq!(f.root_read(), Some(2));
        assert_eq!(r.pending_advance.load(Ordering::Relaxed), 0);
        assert_eq!(r.last_read_offset.load(Ordering::Relaxed), 0x10);
        r.read_offset.store(0x30, Ordering::Relaxed);
        r.pending_advance.store(0x20, Ordering::Relaxed);
        f.rewind();
        assert_eq!(r.read_offset.load(Ordering::Relaxed), 0x10);
        assert_eq!(r.pending_advance.load(Ordering::Relaxed), 0);
        assert_eq!(f.root_read(), Some(2));
    }

    #[test]
    fn child_cursors() {
        let kit = Kit::new();
        kit.features(|x| x.max_fifo_count = 8);
        let m = ChildMem::new(&kit, 1, 1);
        let f = kit.child(m.node_ptr(), false).unwrap();
        let Ring::Child(c) = &f.ring else { panic!() };
        c.read_offset.store(0x18, Ordering::Relaxed);
        c.read_count.store(0x118, Ordering::Relaxed);
        f.latch_offset();
        assert_eq!(f.command_offset.load(Ordering::Relaxed), 0x18);
        f.advance();
        assert_eq!(m.node.read_u32(4), Some(0x118));
        assert_eq!(c.last_read_count.load(Ordering::Relaxed), 0x118);
        assert_eq!(c.last_read_offset.load(Ordering::Relaxed), 0x18);
        c.read_offset.store(0x40, Ordering::Relaxed);
        c.read_count.store(0x140, Ordering::Relaxed);
        f.rewind();
        assert_eq!(c.read_offset.load(Ordering::Relaxed), 0x18);
        assert_eq!(c.read_count.load(Ordering::Relaxed), 0x118);
        assert_eq!(m.node.read_u32(4), Some(0x118));
    }
}

mod root_read_bytes {
    use super::*;

    /// Reads, wraps at the ring end, publishes nothing
    #[test]
    fn reads_and_wraps() {
        let kit = Kit::new();
        let (f, ram) = kit.root(0x500, 0x400); // a 0x100-byte ring
        let Ring::Root(r) = &f.ring else { panic!() };
        for i in 0..0x100u32 {
            ram.write(u64::from(0x400 + i), &[i as u8]);
        }
        r.read_offset.store(0xfc, Ordering::Relaxed);
        f.set_root_written(8);
        let mut b = [0u8; 8];
        assert!(f.read_bytes(&mut b));
        assert_eq!(b, [0xfc, 0xfd, 0xfe, 0xff, 0, 1, 2, 3]);
        assert_eq!(r.read_offset.load(Ordering::Relaxed), 4);
        assert_eq!(r.pending_advance.load(Ordering::Relaxed), 8);
        assert_eq!(f.root_read(), Some(0));
    }

    /// A read ending at the ring end wraps before returning; a zero-byte read performs a
    /// pending wrap
    #[test]
    fn wrap_before_return() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x500, 0x400);
        let Ring::Root(r) = &f.ring else { panic!() };
        r.read_offset.store(0xfc, Ordering::Relaxed);
        f.set_root_written(4);
        let mut b = [0u8; 4];
        assert!(f.read_bytes(&mut b));
        assert_eq!(r.read_offset.load(Ordering::Relaxed), 0);
        r.read_offset.store(0x100, Ordering::Relaxed);
        assert!(f.read_bytes(&mut []));
        assert_eq!(r.read_offset.load(Ordering::Relaxed), 0);
    }

    /// The root reads past `written` once `written != read`; a `written` below `read` wraps
    #[test]
    fn never_waits_inside_a_command() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x500, 0x400);
        let Ring::Root(r) = &f.ring else { panic!() };
        f.set_root_written(1);
        let mut b = [0u8; 0x40];
        assert!(f.read_bytes(&mut b));
        assert_eq!(r.pending_advance.load(Ordering::Relaxed), 0x40);
        r.read.store(10, Ordering::SeqCst);
        f.set_root_written(9);
        assert!(f.read_bytes(&mut b));
    }

    /// Quiesced returns NO, even with nothing left to copy
    #[test]
    fn quiesced_returns_no() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x500, 0x400);
        f.quiesce();
        assert!(!f.read_bytes(&mut []));
        assert!(!f.read_bytes(&mut [0u8; 4]));
    }

    /// Waits while written == read; setWritten: and poke wake it; drain's quiesce ends it
    #[test]
    fn waits_for_written() {
        let kit = Kit::new();
        let (f, ram) = kit.root(0x500, 0x400);
        ram.write(0x400, &[7, 8]);
        let g = f.clone();
        let t = std::thread::spawn(move || {
            let mut b = [0u8; 2];
            let ok = g.read_bytes(&mut b);
            (ok, b)
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        f.poke(); // no bytes yet: it waits again
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!t.is_finished());
        f.set_root_written(2);
        assert_eq!(t.join().unwrap(), (true, [7, 8]));
        // A waiter released by quiesce + notify.
        let g = f.clone();
        let t = std::thread::spawn(move || g.read_bytes(&mut [0u8; 1]));
        std::thread::sleep(std::time::Duration::from_millis(50));
        f.set_root_written(2); // unchanged: written - read still 2; it reads
        let _ = t.join();
    }

    /// A `written` set while the thread submits its held-back work, which submits nothing,
    /// wakes it all the same: the ring is looked at again under the lock before the wait
    #[test]
    fn a_doorbell_in_the_before_block_gap_wakes_it() {
        let kit = Kit::new();
        let (f, ram) = kit.root(0x500, 0x400);
        ram.write(0x400, &[7, 8]);
        let g = f.clone();
        let read = read_past_a_doorbell_in_the_gap(&f, "root gap", 2, move || {
            g.set_root_written(2);
        });
        assert_eq!(
            read,
            Some(vec![7, 8]),
            "the thread slept through the doorbell"
        );
    }

    /// A NULL ring dies at the copy
    #[test]
    fn null_ring_dies() {
        let kit = Kit::new();
        let f = kit.root_at(0x500, GuestPtr::default(), 0x400);
        f.set_root_written(4);
        let r = catch_death(move || f.read_bytes(&mut [0u8; 4]));
        assert_eq!(r, Err(DeathKind::RingNull.site()));
    }
}

/// A child FIFO with its ring mapped (setupMapping run through `start`'s first step).
fn mapped_child(kit: &Kit, pages: u32) -> (Arc<Fifo<P>>, ChildMem) {
    kit.features(|x| x.max_fifo_count = 8);
    let m = ChildMem::new(kit, pages, 1);
    let f = kit.child(m.node_ptr(), false).unwrap();
    assert!(f.map_ring());
    (f, m)
}

mod child_read_bytes {
    use super::*;

    /// Counts locally; waits for the rest of a partial command
    #[test]
    fn waits_for_the_rest() {
        let kit = Kit::new();
        let (f, m) = mapped_child(&kit, 1);
        m.ring.write(0, &[1, 2, 3, 4]);
        m.node.write_u32(0, 2);
        let g = f.clone();
        let t = std::thread::spawn(move || {
            let mut b = [0u8; 4];
            (g.read_bytes(&mut b), b)
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!t.is_finished());
        m.node.write_u32(0, 4);
        f.poke();
        assert_eq!(t.join().unwrap(), (true, [1, 2, 3, 4]));
        let Ring::Child(c) = &f.ring else { panic!() };
        assert_eq!(c.read_count.load(Ordering::Relaxed), 4);
        assert_eq!(c.cache_written.load(Ordering::Relaxed), 4);
        // Nothing published.
        assert_eq!(m.node.read_u32(4), Some(0));
    }

    /// A node's `written` and its poke while the thread submits its held-back work, which
    /// submits nothing, wake it all the same: the node is read again under the lock before the
    /// wait
    #[test]
    fn a_doorbell_in_the_before_block_gap_wakes_it() {
        let kit = Kit::new();
        let (f, m) = mapped_child(&kit, 1);
        m.ring.write(0, &[1, 2, 3, 4]);
        let (g, node) = (f.clone(), Arc::clone(&m.node));
        let read = read_past_a_doorbell_in_the_gap(&f, "child gap", 4, move || {
            node.write_u32(0, 4);
            g.poke();
        });
        assert_eq!(
            read,
            Some(vec![1, 2, 3, 4]),
            "the thread slept through the doorbell"
        );
    }

    /// `written` is reloaded only when the cache is short; a lowered `written` wraps and the
    /// child parses at once
    #[test]
    fn cache_reload_and_lowered_written() {
        let kit = Kit::new();
        let (f, m) = mapped_child(&kit, 1);
        let Ring::Child(c) = &f.ring else { panic!() };
        c.cache_written.store(8, Ordering::Relaxed);
        m.node.write_u32(0, 0); // not reread: the cache covers the request
        assert!(f.read_bytes(&mut [0u8; 4]));
        assert_eq!(c.cache_written.load(Ordering::Relaxed), 8);
        // written lowered below readCount: count wraps huge.
        c.cache_written.store(0, Ordering::Relaxed);
        m.node.write_u32(0, 2);
        assert!(f.read_bytes(&mut [0u8; 16]));
        assert_eq!(c.read_count.load(Ordering::Relaxed), 20);
    }

    /// Wrap, and a torn-down ring dies at the copy
    #[test]
    fn wrap_and_torn_down_ring() {
        let kit = Kit::new();
        let (f, m) = mapped_child(&kit, 1);
        let Ring::Child(c) = &f.ring else { panic!() };
        let last_two = crate::guest::records::APV_PAGE_SIZE - 2;
        m.ring.write(last_two, &[9, 9]);
        m.ring.write(0, &[5]);
        c.read_offset.store(last_two as u32, Ordering::Relaxed);
        m.node.write_u32(0, 3);
        let mut b = [0u8; 3];
        assert!(f.read_bytes(&mut b));
        assert_eq!(b, [9, 9, 5]);
        // Teardown with the ring pointer still in use (the two-thread state).
        let command_length = f.ring_length.load(Ordering::Relaxed);
        f.unmap_ring();
        f.ring_length.store(command_length, Ordering::Relaxed);
        m.node.write_u32(0, 8);
        let r = catch_death(move || f.read_bytes(&mut [0u8; 1]));
        assert_eq!(r, Err(DeathKind::RingNull.site()));
    }
}

mod child_map_ring {
    use super::*;

    /// No list page, an empty list
    #[test]
    fn refusals() {
        let kit = Kit::new();
        kit.features(|x| x.max_fifo_count = 8);
        let m = ChildMem::new(&kit, 0, 1);
        let f = kit.child(m.node_ptr(), false).unwrap();
        assert!(!f.map_ring());
        assert!(!f.map_ring());
        assert_eq!(kit.platform.call_log().count("new_range_mapping"), 0);
        assert_eq!(f.ring_length.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn root_mapping_is_the_devices() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        assert!(f.map_ring());
        f.unmap_ring();
        assert_eq!(f.ring_length.load(Ordering::Relaxed), 0xc00);
    }
}

mod lifecycle {
    use super::*;

    /// Whether `n` of `f`'s threads come to wait for ring bytes within `wait_for`'s time.
    fn waiting_for_bytes(f: &Fifo<P>, n: u32) -> bool {
        wait_for(|| f.waiting_for_bytes.load(Ordering::SeqCst) == n)
    }

    #[test]
    fn root_start_stop_drain() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        f.quiesce();
        assert!(f.start(&kit.platform));
        assert!(!f.is_quiesced());
        assert_eq!(
            kit.platform.fake_threads().spawned(),
            vec![("reims-vgpu.fifo", FIFO_THREAD_STACK)]
        );
        assert!(waiting_for_bytes(&f, 1));
        f.poke();
        f.stop();
        assert!(f.is_quiesced());
        f.drain();
        assert!(!f.running.load(Ordering::Relaxed));
    }

    /// Not quiesced: a death
    #[test]
    fn drain_unquiesced_dies() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        let r = catch_death(move || f.drain());
        assert_eq!(r, Err(DeathKind::DrainUnquiesced.site()));
    }

    #[test]
    fn child_start_marks_node_running() {
        let kit = Kit::new();
        kit.features(|x| x.max_fifo_count = 8);
        let m = ChildMem::new(&kit, 1, 1);
        let f = kit.child(m.node_ptr(), false).unwrap();
        assert!(f.start(&kit.platform));
        assert_eq!(m.node.read_vec(0xe, 2).unwrap(), vec![1, 0]);
        f.stop();
        assert_eq!(m.node.read_vec(0xe, 2).unwrap(), vec![0, 0]);
        f.drain();
        // The thread's exit tore the ring down.
        assert!(f.mapping.lock().is_none());
        assert_eq!(f.ring_length.load(Ordering::Relaxed), 0);
    }

    /// A start over a live thread spawns a second thread, and a child's second mapping keeps
    /// the first for good (`a_second_start_keeps_the_first_ring_for_good`). The two threads share
    /// the cursors: the second start sets the read offset from the node's read count, and leaves
    /// the FIFO's read count where the first thread left it
    #[test]
    fn second_start_second_thread() {
        let kit = Kit::new();
        kit.features(|x| x.max_fifo_count = 8);
        let m = ChildMem::new(&kit, 1, 1);
        let f = kit.child(m.node_ptr(), false).unwrap();
        assert!(f.start(&kit.platform));
        // Each thread waits for bytes before a quiesce. One that met the quiesce on its way in
        // would leave, and its exit clears the running flag the two share: the drain would then
        // wake no one, and the other thread would wait on.
        assert!(waiting_for_bytes(&f, 1));
        f.quiesce(); // no wake: the first thread keeps waiting for bytes
        m.node.write_u32(4, 0x18); // the guest moves the node's read count off the FIFO's (0)
        assert!(f.start(&kit.platform));
        assert_eq!(kit.platform.fake_threads().spawned().len(), 2);
        let Ring::Child(c) = &f.ring else { panic!() };
        assert_eq!(c.read_offset.load(Ordering::Relaxed), 0x18);
        assert_eq!(c.read_count.load(Ordering::Relaxed), 0);
        assert!(waiting_for_bytes(&f, 2));
        // Both threads leave on quiesce + drain's notify_all.
        f.quiesce();
        f.drain();
        assert!(wait_for(|| Arc::strong_count(&f) == 1));
    }

    /// A child's second start while its first thread runs maps the ring again and keeps the
    /// first mapping for good: the first ring's unmap never comes. The second goes at a thread's
    /// exit
    #[test]
    fn a_second_start_keeps_the_first_ring_for_good() {
        let kit = Kit::new();
        kit.features(|x| x.max_fifo_count = 8);
        let m = ChildMem::new(&kit, 1, 1);
        let f = kit.child(m.node_ptr(), false).unwrap();
        assert!(f.start(&kit.platform));
        let first = Arc::downgrade(f.mapping.lock().as_ref().expect("the first ring"));
        // Not quiesced: the first thread waits for bytes, its ring still in place.
        assert!(waiting_for_bytes(&f, 1));
        assert!(f.start(&kit.platform));
        assert_eq!(first.strong_count(), 1);
        assert!(!Weak::ptr_eq(
            &first,
            &Arc::downgrade(f.mapping.lock().as_ref().expect("the second ring"))
        ));
        // Both threads wait for bytes before the quiesce, as in `second_start_second_thread`.
        assert!(waiting_for_bytes(&f, 2));
        f.quiesce();
        f.drain();
        assert!(wait_for(|| Arc::strong_count(&f) == 1));
        assert!(f.mapping.lock().is_none());
        assert_eq!(first.strong_count(), 1);
    }

    /// A single wait on the exit condition
    #[test]
    fn drain_waits_for_thread_exit() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        assert!(f.start(&kit.platform));
        assert!(waiting_for_bytes(&f, 1));
        f.quiesce();
        // The thread waits for ring bytes: drain's notify_all releases it, and drain's single
        // wait returns on the thread's exit notify.
        f.drain();
        assert_eq!(f.waiting_for_bytes.load(Ordering::SeqCst), 0);
        assert!(!f.running.load(Ordering::Relaxed));
    }
}

mod process {
    use super::*;

    /// A root FIFO with a 0x400-byte ring at 0x400, the commands written at the ring
    /// start and `written` set to their total.
    fn run(kit: &Kit, bytes: &[u8]) -> (Arc<Fifo<P>>, Arc<crate::testkit::guest_ram::RawRegion>) {
        let (f, ram) = kit.root(0x800, 0x400);
        ram.write(0x400, bytes);
        assert!(f.start(&kit.platform));
        f.set_root_written(bytes.len() as u32);
        (f, ram)
    }

    /// A bad length is skipped, no stamp, no advance
    #[test]
    fn bad_length_skipped() {
        let kit = Kit::new();
        let logs = crate::testkit::logs::capture();
        let mut bytes = cmd(30, &[], 5, &[], Some(11), None);
        bytes.extend(cmd(30, &[], 6, &[], None, None));
        let (f, _ram) = run(&kit, &bytes);
        assert!(wait_for(|| kit.signals().contains(&(0, 6))));
        // Only one stamp was ever pushed.
        assert_eq!(kit.signals(), vec![(0, 6)]);
        assert_eq!(f.root_read(), Some(24));
        assert_eq!(
            fifo_lines(&logs),
            vec!["channel 0: command 30 of length 11, outside 12..=1024; skipped"]
        );
        shut(&f);
        // A length past the ring is skipped the same way.
        let kit = Kit::new();
        let mut bytes = cmd(30, &[], 5, &[], Some(0x401), None);
        bytes.extend(cmd(30, &[], 6, &[], None, None));
        let (f, _ram) = run(&kit, &bytes);
        assert!(wait_for(|| kit.signals().contains(&(0, 6))));
        assert_eq!(kit.faults(), 0);
        shut(&f);
    }

    /// A child FIFO (which waits for bytes past `written`, unlike the root) running
    /// `bytes`.
    fn run_child(kit: &Kit, bytes: &[u8]) -> (Arc<Fifo<P>>, ChildMem) {
        kit.features(|x| x.max_fifo_count = 8);
        let m = ChildMem::new(kit, 1, 2);
        let f = kit.child(m.node_ptr(), false).unwrap();
        m.ring.write(0, bytes);
        assert!(f.start(&kit.platform));
        m.node.write_u32(0, bytes.len() as u32);
        f.poke();
        (f, m)
    }

    /// An underflow pushes and faults the header's stamp and consumes only the header; a child
    /// pushes into the `last_pending` entry it was given
    #[test]
    fn underflow_faults_header() {
        let kit = Kit::new();
        let logs = crate::testkit::logs::capture();
        // barrierCount 2 but length 12 + 8: underflow.
        let mut bytes = cmd(30, &[], 5, &[0; 8], None, Some(2));
        bytes.extend(cmd(30, &[], 6, &[], None, None));
        let (f, m) = run_child(&kit, &bytes);
        assert!(wait_for(|| kit.faults() == 1));
        // Its line comes before the fault.
        assert_eq!(
            fifo_lines(&logs).first().map(String::as_str),
            Some("channel 2: command 30 of length 20 is too short for its 2 barriers")
        );
        assert_eq!(f.fault_offset(), 0);
        assert_eq!(m.node.read_u32(8), Some(0));
        // Not advanced: the node's readCount stays. The parse went on right after the
        // header: bytes 12..24 read as a zero-length header and skipped, then the last 8
        // bytes copied as the start of the next header while it waits for the rest.
        assert!(wait_for(|| {
            let Ring::Child(c) = &f.ring else { panic!() };
            c.read_count.load(Ordering::Relaxed) == 32
        }));
        assert_eq!(m.node.read_u32(4), Some(0));
        assert_eq!(kit.last_pending.load(Ordering::Relaxed), 5);
        f.stop();
        f.drain();
        // The quiesce published the faulted head.
        assert_eq!(kit.signals(), vec![(2, 5)]);
    }

    /// A barrier raise advances, pushes and faults
    #[test]
    fn barrier_raise_consumes_and_faults() {
        let kit = Kit::new();
        let b = StampWait {
            index: 99,
            value: 2,
        };
        kit.platform.script().push(
            "wait_stamps",
            Step::raise("NSGenericException", "Invalid stamp index"),
        );
        let mut bytes = cmd(30, &[b], 5, &[], None, None);
        let first = bytes.len() as u32;
        bytes.extend(cmd(30, &[], 6, &[], None, None));
        let (f, _ram) = run(&kit, &bytes);
        assert!(wait_for(|| kit.faults() == 1));
        assert!(wait_for(|| f.root_read() == Some(bytes.len() as u32)));
        assert!(f.root_read().unwrap() > first);
        f.resume();
        assert!(wait_for(|| kit.signals().contains(&(0, 6))));
        shut(&f);
    }

    /// The barrier wait's catch logs the raise's detail alone
    #[test]
    fn barrier_raise_logs_the_reason() {
        let kit = Kit::new();
        let b = StampWait {
            index: 99,
            value: 2,
        };
        kit.platform.script().push(
            "wait_stamps",
            Step::raise("NSGenericException", "Invalid stamp index"),
        );
        let bytes = cmd(30, &[b], 5, &[], None, None);
        let logs = crate::testkit::logs::capture();
        let (f, _ram) = run(&kit, &bytes);
        assert!(wait_for(|| kit.faults() == 1));
        assert_eq!(
            fifo_lines(&logs),
            vec![
                "channel 0: command 30's barrier wait failed: stamp barrier index 99 is not below \
                 the FIFO count 64"
            ]
        );
        shut(&f);
    }

    /// The read count publishes before the push, even one that blocks behind 128 pending stamps
    #[test]
    fn advance_before_blocking_push() {
        let kit = Kit::new();
        let mut bytes = Vec::new();
        for i in 0..129u32 {
            bytes.extend(cmd(61, &[], i + 1, &[0; 8], None, None));
        }
        let (f, ram) = kit.root(0x2000, 0x400);
        ram.write(0x400, &bytes);
        assert!(f.start(&kit.platform));
        f.set_root_written(bytes.len() as u32);
        assert!(wait_for(|| f.root_read() == Some(bytes.len() as u32)));
        assert_eq!(kit.last_pending.load(Ordering::Relaxed), 128);
        // Leaked: every stamp stays pending behind the delays.
        f.quiesce();
    }

    /// An invalid opcode faults; the default path
    #[test]
    fn invalid_opcode_faults() {
        let kit = Kit::new();
        let bytes = cmd(13, &[], 5, &[1, 2], None, None);
        let (f, _ram) = run(&kit, &bytes);
        assert!(wait_for(|| kit.faults() == 1));
        assert_eq!(f.root_read(), Some(14));
        shut(&f);
    }

    /// The child thread publishes the node's readCount
    #[test]
    fn child_thread_runs() {
        let kit = Kit::new();
        kit.features(|x| x.max_fifo_count = 8);
        let m = ChildMem::new(&kit, 1, 3);
        let f = kit.child(m.node_ptr(), false).unwrap();
        let bytes = cmd(30, &[], 0x77, &[], None, None);
        m.ring.write(0, &bytes);
        assert!(f.start(&kit.platform));
        m.node.write_u32(0, 12);
        f.poke();
        assert!(wait_for(|| kit.signals().contains(&(3, 0x77))));
        assert_eq!(m.node.read_u32(4), Some(12));
        f.stop();
        f.drain();
    }
}

/// The sink's lifetimes: a signal or a fault loads the FIFO for its call only.
mod sink {
    use super::*;

    /// A signal lets its load go: the owner's reference is the FIFO's last
    #[test]
    fn a_signal_lets_its_load_go() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        let weak = Arc::downgrade(&f);
        f.push_stamp(1, 37);
        f.signal_stamp_value(1);
        assert_eq!(kit.signals(), vec![(0, 1)]);
        assert_eq!(Arc::strong_count(&f), 1);
        drop(f);
        assert!(weak.upgrade().is_none());
        assert!(kit.platform.world().live("MTLSharedEvent").is_empty());
    }

    /// With the FIFO gone a signal sends nothing
    #[test]
    fn signal_block_with_the_fifo_gone_does_nothing() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        let sink = FifoSink {
            fifo: Arc::downgrade(&f),
            stamp_index: ROOT_STAMP_INDEX,
        };
        drop(f);
        sink.signal(1);
        assert!(kit.signals().is_empty());
    }

    /// A fault lets its load go too; the device's asynchronous fault holds its own
    #[test]
    fn a_fault_lets_its_load_go() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        f.push_stamp(1, 37);
        f.fault_at_stamp_value(1);
        assert_eq!(kit.faults(), 1);
        assert_eq!(Arc::strong_count(&f), 1);
    }
}

mod platform_init {
    use super::*;

    /// A refused root init logs
    #[test]
    fn refused_root_init_logs() {
        let kit = Kit::new();
        let logs = crate::testkit::logs::capture();
        let ram = crate::testkit::guest_ram::RawRegion::new(0x4000).unwrap();
        let f = Fifo::try_new_root_on(
            &kit.platform,
            kit.weak(),
            0x4000,
            GuestPtr::new(ram.addr()),
            0x4000,
            kit.last_pending.clone(),
        );
        assert!(f.is_none());
        assert_eq!(
            fifo_lines(&logs),
            vec!["root FIFO refused: start offset 0x4000 is not below its length 0x4000"]
        );
    }

    /// `resume` of a FIFO whose head stamp is not faulted logs the queue's one line
    #[test]
    fn resume_of_an_unfaulted_head_logs() {
        let kit = Kit::new();
        let (f, _ram) = kit.root(0x1000, 0x400);
        let logs = crate::testkit::logs::capture();
        f.resume();
        assert_eq!(
            fifo_lines(&logs),
            vec!["resume: the stamp at the head of the queue is not faulted"]
        );
    }
}
