//! `GuestPtr`: the only raw access to guest RAM. `unsafe` is allowed here and nowhere
//! else in core's product code.
//!
//! The guest writes its memory concurrently, so there is never a Rust reference into it: every
//! access is one volatile or atomic load or store, of the width it names, at the moment the
//! operation reaches it. What the guest sees of each access:
//!
//! - **One instruction per access.** A plain load or store is one `ldr`/`str` of its width,
//!   aligned or not: an unaligned one is the same single instruction (single-copy atomic under
//!   FEAT_LSE2 inside a 16-byte granule), never split into bytes. A `{u32, u32}` pair is one
//!   `stp`/`ldp` and a 16-byte clear one `stp xzr, xzr`, so a guest's wide load sees the old
//!   bytes or the new ones, never half of each.
//! - **The orderings.** A seq_cst load is `ldar`, the one acquire load `ldapr`, a release or
//!   seq_cst store `stlr`, and the one read-modify-write (`fetch_or`) `ldsetal`. Misaligned, each
//!   is that instruction inside a 16-byte granule and a death across one.
//! - **On x86-64** each access is one instruction of the same width, with the same atomicity
//!   inside a 16-byte granule and the same death across one: `raw`'s x86-64 file says which.
//! - **A NULL dereference is a death.** An address inside the zero region (a NULL base, or a NULL
//!   base plus an offset), or one whose bytes wrap past the top of the address space, ends the
//!   process with the death the caller names, before any byte is touched.
//! - **A task that has ended writes nothing.** A write on a thread running the work of an
//!   isolated task that died or was invalidated stops that thread before any byte is touched
//!   ([`stop_if_task_ended`]), as the task's process would have been gone.

// Guest data flows through here: arithmetic is spelt out.
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

use std::sync::atomic::Ordering;

use crate::host::HostPtr;
use crate::num::ToU64;
use crate::raise::death::{DeathId, fatal};
use crate::tasks::isolation::life::stop_if_task_ended;

// The raw accesses, in one file per architecture with the same functions.
#[allow(unsafe_code)] // architecture 17.3: guest/ptr.rs is core's one raw access to guest RAM
#[cfg_attr(target_arch = "aarch64", path = "ptr/raw/aarch64.rs")]
#[cfg_attr(target_arch = "x86_64", path = "ptr/raw/x86_64.rs")]
mod raw;

/// Where the zero region ends: every arm64 macOS executable, QEMU included, maps
/// `__PAGEZERO` over `[0, 4 GiB)` (ld64's default `-pagezero_size`, `otool -l`), so an access below
/// it faults. Name is ours: the "NULL or near-NULL" access.
pub const ZERO_REGION_END: usize = 0x1_0000_0000;

/// The address `base + off`, or the death `death` where the access would fault: the address is
/// inside the zero region (a NULL base, or a NULL range address plus an offset), or `len` bytes
/// from it wrap past the top of the address space.
fn check(base: HostPtr, off: u64, len: usize, death: impl Into<DeathId>) -> HostPtr {
    let a = base.wrapping_add(off);
    if a.addr() < ZERO_REGION_END || a.addr().checked_add(len).is_none() {
        fatal(death)
    }
    a
}

/// Whether `a` is aligned to `n` (a power of two).
fn aligned(a: HostPtr, n: usize) -> bool {
    a.addr() & n.wrapping_sub(1) == 0
}

/// Whether an unaligned atomic (`ldar`, `ldapr`, `stlr`, `ldsetal`) of 4 bytes at `a` stays inside
/// one 16-byte granule, where FEAT_LSE2 (which every Apple arm64 part has:
/// `hw.optional.arm.FEAT_LSE2`) performs it; one that crosses a granule takes an alignment fault.
fn within_granule(a: HostPtr) -> bool {
    (a.addr() & 0xf) <= 0xc
}

/// A host address inside a guest mapping. Copyable: it owns nothing; whoever produced it keeps the
/// mapping alive (a `Mapping`, or a holder that holds one).
///
/// Each access takes `death`, the death when the address is NULL (or near it): the caller names it,
/// since the fault is the caller's own dereference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct GuestPtr {
    addr: HostPtr,
}

impl GuestPtr {
    /// A pointer at `addr` (NULL allowed: the NULL check is at the access, where it dies).
    pub const fn new(addr: HostPtr) -> GuestPtr {
        GuestPtr { addr }
    }

    /// The address.
    pub const fn addr(self) -> HostPtr {
        self.addr
    }

    /// `addr + off`, wrapping (pointer arithmetic, never checked).
    pub const fn offset(self, off: u64) -> GuestPtr {
        GuestPtr {
            addr: self.addr.wrapping_add(off),
        }
    }

    /// Whether the pointer is NULL.
    pub const fn is_null(self) -> bool {
        self.addr.is_null()
    }

    /// A volatile byte load at `self + off`.
    pub fn read_u8(self, off: u64, death: impl Into<DeathId>) -> u8 {
        raw::load_u8(check(self.addr, off, 1, death))
    }

    /// A volatile 16-bit load (unaligned allowed, as arm64 allows it).
    pub fn read_u16(self, off: u64, death: impl Into<DeathId>) -> u16 {
        let a = check(self.addr, off, 2, death);
        if aligned(a, 2) {
            raw::load_u16(a)
        } else {
            raw::load_u16_unaligned(a)
        }
    }

    /// A volatile 32-bit load.
    pub fn read_u32(self, off: u64, death: impl Into<DeathId>) -> u32 {
        let a = check(self.addr, off, 4, death);
        if aligned(a, 4) {
            raw::load_u32(a)
        } else {
            raw::load_u32_unaligned(a)
        }
    }

    /// A volatile 64-bit load.
    pub fn read_u64(self, off: u64, death: impl Into<DeathId>) -> u64 {
        let a = check(self.addr, off, 8, death);
        if aligned(a, 8) {
            raw::load_u64(a)
        } else {
            raw::load_u64_unaligned(a)
        }
    }

    /// A volatile byte store.
    pub fn write_u8(self, off: u64, v: u8, death: impl Into<DeathId>) {
        stop_if_task_ended();
        raw::store_u8(check(self.addr, off, 1, death), v);
    }

    /// A volatile 16-bit store.
    pub fn write_u16(self, off: u64, v: u16, death: impl Into<DeathId>) {
        stop_if_task_ended();
        let a = check(self.addr, off, 2, death);
        if aligned(a, 2) {
            raw::store_u16(a, v);
        } else {
            raw::store_u16_unaligned(a, v);
        }
    }

    /// A volatile 32-bit store.
    pub fn write_u32(self, off: u64, v: u32, death: impl Into<DeathId>) {
        stop_if_task_ended();
        let a = check(self.addr, off, 4, death);
        if aligned(a, 4) {
            raw::store_u32(a, v);
        } else {
            raw::store_u32_unaligned(a, v);
        }
    }

    /// A volatile 64-bit store.
    pub fn write_u64(self, off: u64, v: u64, death: impl Into<DeathId>) {
        stop_if_task_ended();
        let a = check(self.addr, off, 8, death);
        if aligned(a, 8) {
            raw::store_u64(a, v);
        } else {
            raw::store_u64_unaligned(a, v);
        }
    }

    /// A store of a `{u32, u32}` pair (a key and its value) at `self + off`: one `stp wLo, wHi` at
    /// every alignment, so a guest's 64-bit load of a pair inside one 16-byte granule sees the old
    /// pair or the new one, never one word of each. `lo` is the word at `+0`.
    pub fn write_u32_pair(self, off: u64, lo: u32, hi: u32, death: impl Into<DeathId>) {
        stop_if_task_ended();
        raw::stp_u32_pair(check(self.addr, off, 8, death), lo, hi);
    }

    /// A load of a `{u32, u32}` pair at `self + off` with one `ldp wLo, wHi`
    /// (`write_u32_pair`'s atomicity): `(word at +0, word at +4)`.
    pub fn read_u32_pair(self, off: u64, death: impl Into<DeathId>) -> (u32, u32) {
        raw::ldp_u32_pair(check(self.addr, off, 8, death))
    }

    /// A clear of 16 guest bytes at `self + off`: one `stp xzr, xzr`, single-copy atomic at a
    /// 16-byte-aligned address under FEAT_LSE2, so a guest's 16-byte load there sees the old
    /// bytes or zero, never half of each.
    pub fn write_zero_16(self, off: u64, death: impl Into<DeathId>) {
        stop_if_task_ended();
        raw::stp_zero_16(check(self.addr, off, 16, death));
    }

    /// An acquire load of a guest word: the one of the child node's `written`. It is `ldapr`
    /// (RCpc). Every other atomic load of guest memory is seq_cst: use `load_seqcst_u32` there.
    ///
    /// Misaligned (the slots never are), it is what `ldapr` does there: inside a 16-byte granule
    /// FEAT_LSE2 performs it, across one it is an alignment fault, the death `death`.
    pub fn load_acquire_u32(self, off: u64, death: impl Into<DeathId> + Copy) -> u32 {
        let a = check(self.addr, off, 4, death);
        if aligned(a, 4) {
            return raw::atomic_u32(a).load(Ordering::Acquire);
        }
        if !within_granule(a) {
            fatal(death)
        }
        raw::ldapr_u32_unaligned(a)
    }

    /// A seq_cst load of a guest word: `ldar` (RCsc). Unlike `ldapr`, it is never performed
    /// ahead of an earlier `stlr` to another address.
    ///
    /// Misaligned, as `load_acquire_u32`: `ldar` inside a granule, the death across one.
    pub fn load_seqcst_u32(self, off: u64, death: impl Into<DeathId> + Copy) -> u32 {
        let a = check(self.addr, off, 4, death);
        if aligned(a, 4) {
            return raw::atomic_u32(a).load(Ordering::SeqCst);
        }
        if !within_granule(a) {
            fatal(death)
        }
        raw::ldar_u32_unaligned(a)
    }

    /// A release store of a guest word: `stlr`.
    ///
    /// Misaligned, as `load_acquire_u32`: `stlr` inside a granule, the death across one.
    pub fn store_release_u32(self, off: u64, v: u32, death: impl Into<DeathId> + Copy) {
        stop_if_task_ended();
        let a = check(self.addr, off, 4, death);
        if aligned(a, 4) {
            raw::atomic_u32(a).store(v, Ordering::Release);
            return;
        }
        if !within_granule(a) {
            fatal(death)
        }
        raw::stlr_u32_unaligned(a, v);
    }

    /// A seq_cst store of a guest word: `stlr`, the same instruction as a release store, spelled
    /// apart so that each caller names its ordering.
    ///
    /// Misaligned, as `load_acquire_u32`.
    pub fn store_seqcst_u32(self, off: u64, v: u32, death: impl Into<DeathId> + Copy) {
        stop_if_task_ended();
        let a = check(self.addr, off, 4, death);
        if aligned(a, 4) {
            raw::atomic_u32(a).store(v, Ordering::SeqCst);
            return;
        }
        if !within_granule(a) {
            fatal(death)
        }
        raw::stlr_u32_unaligned(a, v);
    }

    /// A seq_cst `fetch_or` of a guest word: one `ldsetal`, a single atomic read-modify-write, so a
    /// bit the guest clears concurrently is never lost or brought back. Returns the old value. The
    /// only read-modify-write of guest memory is the raised-interrupts word's.
    ///
    /// Misaligned, as `load_acquire_u32`: `ldsetal` inside a granule, the death across one.
    pub fn fetch_or_u32(self, off: u64, v: u32, death: impl Into<DeathId> + Copy) -> u32 {
        stop_if_task_ended();
        let a = check(self.addr, off, 4, death);
        if aligned(a, 4) {
            return raw::atomic_u32(a).fetch_or(v, Ordering::SeqCst);
        }
        if !within_granule(a) {
            fatal(death)
        }
        raw::ldsetal_u32_unaligned(a, v)
    }

    /// A bulk copy of `dst.len()` bytes out of guest memory at `self + off` (a `memcpy`).
    ///
    /// A zero-length copy touches nothing and never faults, as `memcpy(dst, NULL, 0)` does not.
    pub fn copy_to(self, off: u64, dst: &mut [u8], death: impl Into<DeathId>) {
        if dst.is_empty() {
            return;
        }
        let a = check(self.addr, off, dst.len(), death);
        let mut i: usize = 0;
        let mut rest = dst;
        // Each a volatile load of the widest naturally aligned unit that fits: 8, 4, 2 or 1
        // bytes. A load never crosses a page, so a fault comes at the same first byte.
        while !rest.is_empty() {
            let at = a.wrapping_add(i.to_u64());
            let taken = std::mem::take(&mut rest);
            if aligned(at, 8) && taken.len() >= 8 {
                let (w, tail) = taken.split_at_mut(8);
                w.copy_from_slice(&raw::load_u64(at).to_ne_bytes());
                rest = tail;
                i = i.wrapping_add(8);
            } else if aligned(at, 4) && taken.len() >= 4 {
                let (w, tail) = taken.split_at_mut(4);
                w.copy_from_slice(&raw::load_u32(at).to_ne_bytes());
                rest = tail;
                i = i.wrapping_add(4);
            } else if aligned(at, 2) && taken.len() >= 2 {
                let (w, tail) = taken.split_at_mut(2);
                w.copy_from_slice(&raw::load_u16(at).to_ne_bytes());
                rest = tail;
                i = i.wrapping_add(2);
            } else if let Some((b, tail)) = taken.split_first_mut() {
                *b = raw::load_u8(at);
                rest = tail;
                i = i.wrapping_add(1);
            }
        }
    }

    /// A bulk copy of `src` into guest memory at `self + off` (a `memcpy`).
    ///
    /// A zero-length copy touches nothing and never faults.
    pub fn copy_from(self, off: u64, src: &[u8], death: impl Into<DeathId>) {
        stop_if_task_ended();
        if src.is_empty() {
            return;
        }
        let a = check(self.addr, off, src.len(), death);
        let mut i: usize = 0;
        let mut rest = src;
        while !rest.is_empty() {
            let at = a.wrapping_add(i.to_u64());
            if aligned(at, 8)
                && let Some((w, tail)) = rest.split_first_chunk::<8>()
            {
                raw::store_u64(at, u64::from_ne_bytes(*w));
                rest = tail;
                i = i.wrapping_add(8);
            } else if let Some((b, tail)) = rest.split_first() {
                raw::store_u8(at, *b);
                rest = tail;
                i = i.wrapping_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::guest::GuestDeath;
    use crate::raise::death::DeathKind;
    use crate::raise::death::DeathSite;
    use crate::testkit::capture::catch_death;
    use crate::testkit::guest_ram::RawRegion;

    /// The death a test's accesses name.
    const SITE: DeathSite = DeathSite {
        kind: DeathKind::NullDeref,
        file: "test.guest_access",
        line: 0,
    };

    fn region() -> (Arc<RawRegion>, GuestPtr) {
        let r = RawRegion::new(0x4000).expect("region");
        let p = GuestPtr::new(r.addr());
        (r, p)
    }

    /// `GuestPtr::new`, `addr`, `offset` (wrapping), `is_null`
    #[test]
    fn pointer_arithmetic_wraps_and_null_is_null() {
        let p = GuestPtr::new(HostPtr::from_addr(0x1000));
        assert_eq!(p.addr(), HostPtr::from_addr(0x1000));
        assert_eq!(p.offset(0x10).addr(), HostPtr::from_addr(0x1010));
        assert!(!p.is_null());
        assert!(GuestPtr::default().is_null());
        let top = GuestPtr::new(HostPtr::from_addr(usize::MAX));
        assert_eq!(top.offset(1).addr(), HostPtr::NULL);
    }

    /// `read_u8`, `write_u8`
    #[test]
    fn bytes_round_trip() {
        let (r, p) = region();
        p.write_u8(3, 0xab, SITE);
        assert_eq!(p.read_u8(3, SITE), 0xab);
        assert_eq!(r.read_vec(3, 1), Some(vec![0xab]));
    }

    /// `read_u16`/`write_u16`, aligned and unaligned, little-endian
    #[test]
    fn u16_aligned_and_unaligned() {
        let (r, p) = region();
        p.write_u16(2, 0x1234, SITE);
        p.write_u16(5, 0xbeef, SITE);
        assert_eq!(p.read_u16(2, SITE), 0x1234);
        assert_eq!(p.read_u16(5, SITE), 0xbeef);
        assert_eq!(r.read_vec(2, 5), Some(vec![0x34, 0x12, 0, 0xef, 0xbe]));
    }

    /// `read_u32`/`write_u32`, aligned and unaligned
    #[test]
    fn u32_aligned_and_unaligned() {
        let (r, p) = region();
        p.write_u32(8, 0xdead_beef, SITE);
        p.write_u32(13, 0x0102_0304, SITE);
        assert_eq!(p.read_u32(8, SITE), 0xdead_beef);
        assert_eq!(p.read_u32(13, SITE), 0x0102_0304);
        assert_eq!(r.read_vec(13, 4), Some(vec![4, 3, 2, 1]));
        assert_eq!(r.read_u32(8), Some(0xdead_beef));
    }

    /// `read_u64`/`write_u64`, aligned and unaligned
    #[test]
    fn u64_aligned_and_unaligned() {
        let (r, p) = region();
        p.write_u64(16, 0x0102_0304_0506_0708, SITE);
        p.write_u64(0x21, 0x1122_3344_5566_7788, SITE);
        assert_eq!(p.read_u64(16, SITE), 0x0102_0304_0506_0708);
        assert_eq!(p.read_u64(0x21, SITE), 0x1122_3344_5566_7788);
        assert_eq!(r.read_u64(16), Some(0x0102_0304_0506_0708));
        assert_eq!(
            r.read_vec(0x21, 8),
            Some(vec![0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11])
        );
    }

    /// `load_acquire_u32`/`store_release_u32`, aligned
    #[test]
    fn atomics_aligned() {
        let (r, p) = region();
        p.store_release_u32(0x40, 7, SITE);
        assert_eq!(r.read_u32(0x40), Some(7));
        assert!(r.write_u32(0x44, 9));
        assert_eq!(p.load_acquire_u32(0x44, SITE), 9);
    }

    /// `load_seqcst_u32`/`store_seqcst_u32`/`fetch_or_u32`, aligned (the seq_cst guest atomics)
    #[test]
    fn seqcst_atomics_aligned() {
        let (r, p) = region();
        p.store_seqcst_u32(0x100, 0x2, SITE);
        assert_eq!(r.read_u32(0x100), Some(0x2));
        assert!(r.write_u32(0x104, 0xe));
        assert_eq!(p.load_seqcst_u32(0x104, SITE), 0xe);
        assert_eq!(p.fetch_or_u32(0x100, 0x8, SITE), 0x2);
        assert_eq!(p.fetch_or_u32(0x100, 0x2, SITE), 0xa);
        assert_eq!(r.read_u32(0x100), Some(0xa));
    }

    /// The raised-interrupts OR is one atomic RMW: a bit the guest clears concurrently is never
    /// brought back by the host's OR of another bit
    #[test]
    fn fetch_or_never_resurrects_a_concurrent_guest_clear() {
        use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
        let (r, p) = region();
        let stop = AtomicBool::new(false);
        let resurrected = AtomicU64::new(0);
        let cell = p.addr();
        std::thread::scope(|sc| {
            sc.spawn(|| {
                // SAFETY (test): the region outlives the scope; the guest's own atomic view.
                #[allow(unsafe_code)]
                let c = unsafe { AtomicU32::from_ptr(cell.as_ptr::<u32>()) };
                // Bit 1 is the guest's alone: once its own clear returns, only a lost update by
                // the host can set it again before the guest does.
                while !stop.load(Ordering::Relaxed) {
                    c.fetch_or(2, Ordering::SeqCst);
                    c.fetch_and(!2, Ordering::SeqCst);
                    if c.load(Ordering::SeqCst) & 2 != 0 {
                        resurrected.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
            for _ in 0..1_000_000 {
                p.fetch_or_u32(0, 8, SITE);
            }
            stop.store(true, Ordering::Relaxed);
        });
        assert_eq!(resurrected.load(Ordering::Relaxed), 0);
        assert_eq!(p.load_seqcst_u32(0, SITE), 8);
        drop(r);
    }

    /// An unaligned plain 64-bit store inside one 16-byte granule is one `str` (single-copy atomic
    /// under FEAT_LSE2, as the store to the task root's +4 is), never torn into bytes a concurrent
    /// reader can mix
    #[test]
    fn unaligned_u64_in_granule_never_tears() {
        use std::sync::atomic::AtomicBool;
        let (r, p) = region();
        let stop = AtomicBool::new(false);
        std::thread::scope(|sc| {
            sc.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    p.write_u64(4, 0, SITE);
                    p.write_u64(4, u64::MAX, SITE);
                }
            });
            for _ in 0..1_000_000 {
                let v = p.read_u64(4, SITE);
                assert!(v == 0 || v == u64::MAX, "torn: {v:#x}");
            }
            stop.store(true, Ordering::Relaxed);
        });
        drop(r);
    }

    /// Misaligned atomics inside a 16-byte granule act, across one they die the death named
    #[test]
    fn atomics_misaligned() {
        let (r, p) = region();
        p.store_release_u32(0x41, 0xa1b2_c3d4, SITE);
        assert_eq!(r.read_vec(0x41, 4), Some(vec![0xd4, 0xc3, 0xb2, 0xa1]));
        assert_eq!(p.load_acquire_u32(0x41, SITE), 0xa1b2_c3d4);
        assert_eq!(p.load_acquire_u32(0x4c, SITE), 0);
        assert_eq!(catch_death(|| p.load_acquire_u32(0x4d, SITE)), Err(SITE));
        assert_eq!(
            catch_death(|| p.store_release_u32(0x4f, 1, SITE)),
            Err(SITE)
        );
        assert_eq!(r.read_vec(0x4f, 4), Some(vec![0, 0, 0, 0]));
        p.store_seqcst_u32(0x51, 0x0102_0304, SITE);
        assert_eq!(r.read_vec(0x51, 4), Some(vec![4, 3, 2, 1]));
        assert_eq!(p.load_seqcst_u32(0x51, SITE), 0x0102_0304);
        assert_eq!(p.fetch_or_u32(0x51, 0xf000_0000, SITE), 0x0102_0304);
        assert_eq!(p.load_seqcst_u32(0x51, SITE), 0xf102_0304);
        assert_eq!(catch_death(|| p.load_seqcst_u32(0x5d, SITE)), Err(SITE));
        assert_eq!(catch_death(|| p.store_seqcst_u32(0x5e, 1, SITE)), Err(SITE));
        assert_eq!(catch_death(|| p.fetch_or_u32(0x5f, 1, SITE)), Err(SITE));
        assert_eq!(r.read_vec(0x5d, 3), Some(vec![0, 0, 0]));
    }

    /// `copy_to`/`copy_from` at every alignment and length around the word size
    #[test]
    fn bulk_copies_every_alignment() {
        let (r, p) = region();
        for start in 0u64..9 {
            for len in 0usize..20 {
                let src: Vec<u8> = (0..len).map(|i| u8::try_from(i + 1).unwrap()).collect();
                assert!(r.zero(0, 64));
                p.copy_from(start, &src, SITE);
                let mut back = vec![0u8; len];
                p.copy_to(start, &mut back, SITE);
                assert_eq!(back, src);
                let all = r.read_vec(0, 64).unwrap();
                let s = usize::try_from(start).unwrap();
                assert!(all[..s].iter().all(|b| *b == 0));
                assert_eq!(&all[s..s + len], &src[..]);
                assert!(all[s + len..].iter().all(|b| *b == 0));
            }
        }
    }

    /// `write_u32_pair`/`read_u32_pair` at every alignment: the first word at +0, the second at +4,
    /// nothing else touched; `write_zero_16` zeroes exactly 16 bytes
    #[test]
    fn pairs_and_zero_16_every_alignment() {
        let (r, p) = region();
        for start in 0u64..17 {
            assert!(r.zero(0, 64));
            p.write_u32_pair(start, 0x0403_0201, 0x0807_0605, SITE);
            assert_eq!(p.read_u32_pair(start, SITE), (0x0403_0201, 0x0807_0605));
            let all = r.read_vec(0, 64).unwrap();
            let s = usize::try_from(start).unwrap();
            assert!(all[..s].iter().all(|b| *b == 0));
            assert_eq!(&all[s..s + 8], &[1, 2, 3, 4, 5, 6, 7, 8]);
            assert!(all[s + 8..].iter().all(|b| *b == 0));
            let ones: Vec<u8> = vec![0xff; 64];
            p.copy_from(0, &ones, SITE);
            p.write_zero_16(start, SITE);
            let all = r.read_vec(0, 64).unwrap();
            assert!(all[..s].iter().all(|b| *b == 0xff));
            assert!(all[s..s + 16].iter().all(|b| *b == 0));
            assert!(all[s + 16..].iter().all(|b| *b == 0xff));
        }
    }

    /// A pair stored by one `stp` inside a 16-byte granule is never seen torn by a guest's one
    /// 64-bit load, and one `ldp` never mixes two of a guest's 64-bit stores; a 16-byte zero at an
    /// aligned address is never seen half done by a guest's one 16-byte load
    #[test]
    fn pairs_and_zero_16_are_single_copy_atomic_in_a_granule() {
        use std::sync::atomic::AtomicBool;
        let (r, p) = region();
        let stop = AtomicBool::new(false);
        std::thread::scope(|sc| {
            sc.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    p.write_u32_pair(4, 1, 2, SITE);
                    p.write_u32_pair(4, 3, 4, SITE);
                    p.write_u64(0x24, 0x0000_0006_0000_0005, SITE);
                    p.write_u64(0x24, 0x0000_0008_0000_0007, SITE);
                }
            });
            for _ in 0..200_000 {
                let v = p.read_u64(4, SITE);
                assert!(
                    v == 0 || v == 0x0000_0002_0000_0001 || v == 0x0000_0004_0000_0003,
                    "torn store: {v:#x}"
                );
                let pair = p.read_u32_pair(0x24, SITE);
                assert!(
                    pair == (0, 0) || pair == (5, 6) || pair == (7, 8),
                    "torn load: {pair:?}"
                );
            }
            stop.store(true, Ordering::Relaxed);
        });
        // A guest's 16-byte store and load (single-copy atomic at a 16-aligned address).
        let stop = AtomicBool::new(false);
        std::thread::scope(|sc| {
            sc.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    assert!(r.store_16_atomic(0x40, u64::MAX, u64::MAX));
                }
            });
            for _ in 0..200_000 {
                p.write_zero_16(0x40, SITE);
                let (x, y) = r.load_16_atomic(0x40).unwrap();
                assert_eq!(x, y, "a 16-byte zero seen half done");
            }
            stop.store(true, Ordering::Relaxed);
        });
        drop(r);
    }

    /// Every access dies the death its caller names on a NULL base (a NULL dereference)
    #[test]
    fn null_base_dies_at_site() {
        let p = GuestPtr::default();
        let mut buf = [0u8; 4];
        let deaths: [Box<dyn Fn()>; 16] = [
            Box::new(|| p.write_u32_pair(0, 1, 2, SITE)),
            Box::new(|| {
                let _ = p.read_u32_pair(0, SITE);
            }),
            Box::new(|| p.write_zero_16(0, SITE)),
            Box::new(|| {
                let _ = p.read_u8(0, SITE);
            }),
            Box::new(|| {
                let _ = p.read_u16(0, SITE);
            }),
            Box::new(|| {
                let _ = p.read_u32(0, SITE);
            }),
            Box::new(|| {
                let _ = p.read_u64(0, SITE);
            }),
            Box::new(|| p.write_u8(0, 1, SITE)),
            Box::new(|| p.write_u16(0, 1, SITE)),
            Box::new(|| p.write_u32(0, 1, SITE)),
            Box::new(|| p.write_u64(0, 1, SITE)),
            Box::new(|| {
                let _ = p.load_acquire_u32(0, SITE);
            }),
            Box::new(|| p.store_release_u32(0, 1, SITE)),
            Box::new(|| {
                let _ = p.load_seqcst_u32(0, SITE);
            }),
            Box::new(|| p.store_seqcst_u32(0, 1, SITE)),
            Box::new(|| {
                let _ = p.fetch_or_u32(0x100, 1, SITE);
            }),
        ];
        for d in deaths {
            assert_eq!(catch_death(d), Err(SITE));
        }
        assert_eq!(catch_death(|| p.copy_to(0, &mut buf, SITE)), Err(SITE));
        assert_eq!(catch_death(|| p.copy_from(0, &buf, SITE)), Err(SITE));
    }

    /// A near-NULL address (a NULL range address plus an offset) dies, up to the end of
    /// `__PAGEZERO`; the death passed through is the one reported
    #[test]
    fn near_null_dies_until_zero_region_end() {
        let site = DeathSite {
            kind: DeathKind::NullDeref,
            file: "test.near_null",
            line: 0,
        };
        let p = GuestPtr::default();
        assert_eq!(catch_death(|| p.read_u32(0x14, site)), Err(site));
        let last = u64::try_from(ZERO_REGION_END - 1).unwrap();
        assert_eq!(catch_death(|| p.read_u8(last, site)), Err(site));
        assert_eq!(
            catch_death(|| GuestPtr::new(HostPtr::from_addr(0x4000)).write_u8(0, 1, site)),
            Err(site)
        );
    }

    /// A death given as an area's typed id is the one reported
    #[test]
    fn a_typed_death_is_the_one_reported() {
        let p = GuestPtr::default();
        let death = GuestDeath::UnknownInfoKey;
        assert_eq!(catch_death(|| p.read_u32(0, death)), Err(death.site()));
        assert_eq!(
            catch_death(|| p.fetch_or_u32(0, 1, death)),
            Err(death.site())
        );
    }

    /// An address that wraps past the top of the address space dies
    #[test]
    fn wrapping_access_dies() {
        let p = GuestPtr::new(HostPtr::from_addr(usize::MAX - 1));
        assert_eq!(catch_death(|| p.read_u32(0, SITE)), Err(SITE));
        // A real base plus an offset that wraps back into the zero region (`p + off`).
        let (_r, q) = region();
        let base = u64::try_from(q.addr().addr()).unwrap();
        let off = (u64::MAX - base) + 0x11;
        assert_eq!(q.offset(off).addr(), HostPtr::from_addr(0x10));
        assert_eq!(catch_death(|| q.read_u8(off, SITE)), Err(SITE));
    }

    /// A zero-length `memcpy` from or to NULL touches nothing and does not die
    #[test]
    fn empty_copies_never_fault() {
        let p = GuestPtr::default();
        assert_eq!(catch_death(|| p.copy_to(0, &mut [], SITE)), Ok(()));
        assert_eq!(catch_death(|| p.copy_from(0, &[], SITE)), Ok(()));
    }
}
