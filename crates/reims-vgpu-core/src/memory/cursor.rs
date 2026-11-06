//! A read (and write) position over a list of host ranges: how commands, replies and task
//! memory are read and written.

use std::any::Any;
use std::sync::Arc;

use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::guest::GuestPtr;
use crate::host::HostPtr;
use crate::memory::{DeathKind, RaiseKind};
use crate::num::{ToU64, ToUsize};
use crate::raise::{OnFault, RaiseResult};

/// A host range the cursor reads and writes through `guest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CursorRange {
    /// Where the range starts.
    pub address: HostPtr,
    /// Its length.
    pub length: u64,
}

/// A cursor's error message (the callers log or raise it).
pub type CursorError = String;

/// A read position over a shared list of host ranges. Clones share the ranges; `truncate_to`
/// changes only the one cursor. A pointer cursor keeps its buffer alive through `keep_alive`,
/// which the last clone's drop releases.
#[derive(Clone, Default)]
pub struct Cursor {
    ranges: Arc<Vec<CursorRange>>,
    range_index: u64,
    range_offset: u64,
    length: u64,
    offset: u64,
    keep_alive: Option<Arc<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cursor")
            .field("ranges", &self.ranges)
            .field("range_index", &self.range_index)
            .field("range_offset", &self.range_offset)
            .field("length", &self.length)
            .field("offset", &self.offset)
            .finish_non_exhaustive()
    }
}

/// Whether `length` bytes fit from `offset` in a cursor of `cursor_length`.
fn validate_operation_size(
    offset: u64,
    cursor_length: u64,
    length: u64,
    operation: &str,
) -> Result<(), CursorError> {
    let (end, overflow) = offset.overflowing_add(length);
    if overflow || length > cursor_length || end > cursor_length {
        return Err(format!(
            "{operation} of {length:#x} bytes at offset {offset:#x} runs past the cursor's \
             {cursor_length:#x} bytes"
        ));
    }
    Ok(())
}

/// Whether `length` bytes fit from `offset + at` in a cursor of `cursor_length`.
fn validate_operation_size_at(
    offset: u64,
    cursor_length: u64,
    length: u64,
    at: u64,
    operation: &str,
) -> Result<(), CursorError> {
    let (start, overflow) = offset.overflowing_add(at);
    let failed = overflow || {
        let (end, overflow) = start.overflowing_add(length);
        overflow || length > cursor_length || end > cursor_length
    };
    if failed {
        return Err(format!(
            "{operation} of {length:#x} bytes at offset {offset:#x}+{at:#x} runs past the \
             cursor's {cursor_length:#x} bytes"
        ));
    }
    Ok(())
}

/// The next `step` bytes of `buf`, advancing it. `step` never exceeds what is left, since the
/// count of bytes left counts down with it.
fn take_mut<'a>(buf: &mut &'a mut [u8], step: u64) -> &'a mut [u8] {
    match std::mem::take(buf).split_at_mut_checked(step.to_usize()) {
        Some((head, tail)) => {
            *buf = tail;
            head
        }
        None => crate::fatal!(DeathKind::CursorBufferEnd),
    }
}

/// [`take_mut`] for a source buffer.
fn take<'a>(buf: &mut &'a [u8], step: u64) -> &'a [u8] {
    match buf.split_at_checked(step.to_usize()) {
        Some((head, tail)) => {
            *buf = tail;
            head
        }
        None => crate::fatal!(DeathKind::CursorBufferEnd),
    }
}

impl Cursor {
    /// Over `ranges`: the length is the checked sum of the ranges, 0 on overflow.
    fn from_ranges(ranges: Arc<Vec<CursorRange>>) -> Cursor {
        let mut length: u64 = 0;
        for range in ranges.iter() {
            let (sum, overflow) = length.overflowing_add(range.length);
            if overflow {
                length = 0;
                break;
            }
            length = sum;
        }
        Cursor {
            ranges,
            range_index: 0,
            range_offset: 0,
            length,
            offset: 0,
            keep_alive: None,
        }
    }

    /// Over `length` bytes at `pointer`, behind one empty range.
    fn from_pointer(
        keep_alive: Arc<dyn Any + Send + Sync>,
        pointer: HostPtr,
        length: u64,
    ) -> Cursor {
        // One value-initialised {NULL, 0} range ahead of the pointer's.
        let ranges = vec![
            CursorRange::default(),
            CursorRange {
                address: pointer,
                length,
            },
        ];
        Cursor {
            ranges: Arc::new(ranges),
            range_index: 0,
            range_offset: 0,
            length,
            offset: 0,
            keep_alive: Some(keep_alive),
        }
    }

    /// The child covers the parent's next `sub_length` bytes, and the parent advances past
    /// them.
    fn child_of(parent: &mut Cursor, sub_length: u64) -> Cursor {
        let child = Cursor {
            ranges: Arc::clone(&parent.ranges),
            range_index: parent.range_index,
            range_offset: parent.range_offset,
            length: sub_length,
            offset: 0,
            keep_alive: parent.keep_alive.clone(),
        };
        parent.advance(sub_length);
        child
    }

    /// The child's length is `at + sub_length` (wrapping), and it advances by `at`.
    fn child_at(other: &Cursor, at: u64, sub_length: u64) -> Cursor {
        let mut child = Cursor {
            ranges: Arc::clone(&other.ranges),
            range_index: other.range_index,
            range_offset: other.range_offset,
            length: at.wrapping_add(sub_length),
            offset: 0,
            keep_alive: other.keep_alive.clone(),
        };
        child.advance(at);
        child
    }

    /// Range `index`. Past the list it dies (a checked `at`); the lengths make it unreachable.
    fn range_at(&self, index: u64) -> CursorRange {
        match self.ranges.get(index.to_usize()) {
            Some(r) => *r,
            None => crate::fatal!(DeathKind::CursorRangeIndex),
        }
    }

    /// Over `ranges`, moved into a new shared list.
    pub fn with_ranges(ranges: Vec<CursorRange>) -> Cursor {
        Cursor::from_ranges(Arc::new(ranges))
    }

    /// Over `length` bytes at `pointer`, which `keep_alive` owns (a leading empty `{NULL, 0}` range
    /// included).
    pub fn with_pointer(
        keep_alive: Arc<dyn Any + Send + Sync>,
        pointer: HostPtr,
        length: u64,
    ) -> Cursor {
        Cursor::from_pointer(keep_alive, pointer, length)
    }

    /// Moves `bytes` on, clamping silently at the end.
    pub fn advance(&mut self, bytes: u64) {
        let (new_offset, overflow) = self.offset.overflowing_add(bytes);
        if overflow || new_offset >= self.length {
            // Saturates at the end without walking the ranges.
            self.offset = self.length;
            return;
        }
        let mut bytes = bytes;
        while bytes != 0 {
            let range = self.range_at(self.range_index);
            let step = bytes.min(range.length.wrapping_sub(self.range_offset));
            self.range_offset = self.range_offset.wrapping_add(step);
            if range.length == self.range_offset {
                self.range_index = self.range_index.wrapping_add(1);
                self.range_offset = 0;
            }
            bytes = bytes.wrapping_sub(step);
        }
        self.offset = new_offset;
    }

    /// Reads `into.len()` bytes and moves past them; a refused read moves nothing. "into must be
    /// non-null" cannot arise: a slice is never NULL.
    pub fn read_bytes(&mut self, into: &mut [u8]) -> Result<(), CursorError> {
        let mut bytes = into.len().to_u64();
        validate_operation_size(self.offset, self.length, bytes, "read")?;
        // A read inside the current range, short of its end, as nearly every record's is: one
        // copy, and the cursor moves as the loop below would move it.
        if let Some(range) = self.ranges.get(self.range_index.to_usize())
            && bytes < range.length.wrapping_sub(self.range_offset)
        {
            GuestPtr::new(range.address.wrapping_add(self.range_offset)).copy_to(
                0,
                into,
                DeathKind::CursorReadNull,
            );
            self.range_offset = self.range_offset.wrapping_add(bytes);
            self.offset = self.offset.wrapping_add(bytes);
            return Ok(());
        }
        let mut into = into;
        while bytes != 0 {
            let range = self.range_at(self.range_index);
            let step = bytes.min(range.length.wrapping_sub(self.range_offset));
            let dst = take_mut(&mut into, step);
            // A copy of 0 bytes touches nothing.
            // The address is summed as an integer; NULL faults at the copy.
            if step != 0 {
                GuestPtr::new(range.address.wrapping_add(self.range_offset)).copy_to(
                    0,
                    dst,
                    DeathKind::CursorReadNull,
                );
            }
            self.range_offset = self.range_offset.wrapping_add(step);
            if range.length == self.range_offset {
                self.range_index = self.range_index.wrapping_add(1);
                self.range_offset = 0;
            }
            self.offset = self.offset.wrapping_add(step);
            bytes = bytes.wrapping_sub(step);
        }
        Ok(())
    }

    /// Reads `into.len()` bytes `at` bytes on; the cursor does not move.
    pub fn read_bytes_at(&self, at: u64, into: &mut [u8]) -> Result<(), CursorError> {
        let mut bytes = into.len().to_u64();
        validate_operation_size_at(self.offset, self.length, bytes, at, "read")?;
        let mut into = into;
        let mut at = at;
        let mut index = self.range_index;
        let mut position = self.range_offset;
        while bytes != 0 {
            let range = self.range_at(index);
            position = position.wrapping_add(at);
            if position > range.length {
                at = position.wrapping_sub(range.length);
            } else {
                let step = bytes.min(range.length.wrapping_sub(position));
                let dst = take_mut(&mut into, step);
                if step != 0 {
                    GuestPtr::new(range.address.wrapping_add(position)).copy_to(
                        0,
                        dst,
                        DeathKind::CursorReadNull,
                    );
                }
                at = 0;
                bytes = bytes.wrapping_sub(step);
            }
            position = 0;
            index = index.wrapping_add(1);
        }
        Ok(())
    }

    /// Into a zeroed `T`.
    pub fn read<T: FromBytes + IntoBytes>(&mut self) -> Result<T, CursorError> {
        let mut value = T::new_zeroed();
        self.read_bytes(value.as_mut_bytes())?;
        Ok(value)
    }

    /// A `T` `at` bytes on, into a zeroed `T`; the cursor does not move.
    pub fn read_at<T: FromBytes + IntoBytes>(&self, at: u64) -> Result<T, CursorError> {
        let mut value = T::new_zeroed();
        self.read_bytes_at(at, value.as_mut_bytes())?;
        Ok(value)
    }

    /// A `T`, or the raise "record not readable: …" (a command record shorter than its type). A
    /// failed read copies nothing and returns nothing. The raise is as `on` says: the
    /// deserializer reads records under a handler, and the rate-map reset job with none.
    pub fn read_or_raise<T: FromBytes + IntoBytes>(&mut self, on: OnFault) -> RaiseResult<T> {
        let mut value = T::new_zeroed();
        match self.read_bytes(value.as_mut_bytes()) {
            Ok(()) => Ok(value),
            Err(error) => crate::raise!(on => RaiseKind::CursorReadFailed,
                format_args!("record not readable: {error}")
            ),
        }
    }

    /// A cursor over the next `sub_length` bytes; this cursor advances past them.
    pub fn sub_cursor(&mut self, sub_length: u64) -> Result<Cursor, CursorError> {
        let remaining = self.remaining();
        if remaining < sub_length {
            return Err(format!(
                "a sub-cursor of {sub_length:#x} bytes, with {remaining:#x} left"
            ));
        }
        if self.offset.overflowing_add(sub_length).1 {
            // Unreachable after the check above.
            return Err(format!("a sub-cursor of {sub_length:#x} bytes overflows"));
        }
        Ok(Cursor::child_of(self, sub_length))
    }

    /// A cursor over `sub_length` bytes `at` bytes on; this cursor does not move.
    pub fn sub_cursor_at(&self, at: u64, sub_length: u64) -> Result<Cursor, CursorError> {
        let (start, overflow) = at.overflowing_add(self.offset);
        let (end, overflow) = if overflow {
            (0, true)
        } else {
            start.overflowing_add(sub_length)
        };
        if overflow {
            return Err(format!(
                "a sub-cursor of {sub_length:#x} bytes at {at:#x} overflows"
            ));
        }
        if end > self.length {
            return Err(format!(
                "a sub-cursor of {sub_length:#x} bytes at {at:#x}, with {:#x} left",
                self.remaining()
            ));
        }
        Ok(Cursor::child_at(self, at, sub_length))
    }

    /// Sets the absolute length of this cursor only (clones share the ranges, not the length).
    pub fn truncate_to(&mut self, new_length: u64) -> Result<(), CursorError> {
        if new_length > self.length {
            return Err(format!(
                "length {new_length:#x} is past the cursor's {:#x}",
                self.length
            ));
        }
        if new_length < self.offset {
            return Err(format!(
                "length {new_length:#x} of a cursor of {:#x} is before its offset {:#x}",
                self.length, self.offset
            ));
        }
        self.length = new_length;
        Ok(())
    }

    /// `length - offset` (wrapping).
    pub fn remaining(&self) -> u64 {
        self.length.wrapping_sub(self.offset)
    }
}

/// The same cursor with writes.
#[derive(Clone, Default, Debug)]
pub struct WritableCursor(pub Cursor);

impl WritableCursor {
    /// Over `ranges`, as [`Cursor::with_ranges`].
    pub fn with_ranges(ranges: Vec<CursorRange>) -> WritableCursor {
        WritableCursor(Cursor::with_ranges(ranges))
    }

    /// Over `length` bytes at `pointer`, as [`Cursor::with_pointer`].
    pub fn with_pointer(
        keep_alive: Arc<dyn Any + Send + Sync>,
        pointer: HostPtr,
        length: u64,
    ) -> WritableCursor {
        WritableCursor(Cursor::with_pointer(keep_alive, pointer, length))
    }

    /// Writes `from` and moves past it; a refused write moves nothing. "from must be non-null"
    /// cannot arise: a slice is never NULL.
    pub fn write_bytes(&mut self, from: &[u8]) -> Result<(), CursorError> {
        let c = &mut self.0;
        let mut bytes = from.len().to_u64();
        validate_operation_size(c.offset, c.length, bytes, "write")?;
        let mut from = from;
        while bytes != 0 {
            let range = c.range_at(c.range_index);
            let step = bytes.min(range.length.wrapping_sub(c.range_offset));
            let src = take(&mut from, step);
            if step != 0 {
                GuestPtr::new(range.address.wrapping_add(c.range_offset)).copy_from(
                    0,
                    src,
                    DeathKind::CursorWriteNull,
                );
            }
            c.range_offset = c.range_offset.wrapping_add(step);
            if range.length == c.range_offset {
                c.range_index = c.range_index.wrapping_add(1);
                c.range_offset = 0;
            }
            c.offset = c.offset.wrapping_add(step);
            bytes = bytes.wrapping_sub(step);
        }
        Ok(())
    }

    /// Writes `from` `at` bytes on; the cursor does not move.
    pub fn write_bytes_at(&mut self, at: u64, from: &[u8]) -> Result<(), CursorError> {
        let c = &self.0;
        let mut bytes = from.len().to_u64();
        validate_operation_size_at(c.offset, c.length, bytes, at, "write")?;
        let mut from = from;
        let mut at = at;
        let mut index = c.range_index;
        let mut position = c.range_offset;
        while bytes != 0 {
            let range = c.range_at(index);
            position = position.wrapping_add(at);
            if position > range.length {
                at = position.wrapping_sub(range.length);
            } else {
                let step = bytes.min(range.length.wrapping_sub(position));
                let src = take(&mut from, step);
                if step != 0 {
                    GuestPtr::new(range.address.wrapping_add(position)).copy_from(
                        0,
                        src,
                        DeathKind::CursorWriteNull,
                    );
                }
                at = 0;
                bytes = bytes.wrapping_sub(step);
            }
            position = 0;
            index = index.wrapping_add(1);
        }
        Ok(())
    }

    /// `sizeof(T)` bytes, tail padding included (the caller's `T` holds them).
    pub fn write<T: IntoBytes + Immutable>(&mut self, value: &T) -> Result<(), CursorError> {
        self.write_bytes(value.as_bytes())
    }

    /// `value`'s bytes `at` bytes on; the cursor does not move.
    pub fn write_at<T: IntoBytes + Immutable>(
        &mut self,
        at: u64,
        value: &T,
    ) -> Result<(), CursorError> {
        self.write_bytes_at(at, value.as_bytes())
    }

    /// A writable cursor over the next `sub_length` bytes; this cursor advances past them.
    pub fn writable_sub_cursor(&mut self, sub_length: u64) -> Result<WritableCursor, CursorError> {
        self.0.sub_cursor(sub_length).map(WritableCursor)
    }

    /// A writable cursor over `sub_length` bytes `at` bytes on.
    pub fn writable_sub_cursor_at(
        &mut self,
        at: u64,
        sub_length: u64,
    ) -> Result<WritableCursor, CursorError> {
        self.0.sub_cursor_at(at, sub_length).map(WritableCursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raise::RaiseId;
    use crate::testkit::capture::catch_death;

    /// Host memory for the cursor's ranges: kept alive by the test.
    struct Mem(Vec<u8>);

    impl Mem {
        fn new(len: usize, seed: u8) -> Mem {
            Mem((0..len).map(|i| (i as u8).wrapping_add(seed)).collect())
        }

        fn range(&mut self, from: usize, len: u64) -> CursorRange {
            CursorRange {
                address: HostPtr::from_ptr(self.0.as_mut_ptr()).wrapping_add(from as u64),
                length: len,
            }
        }
    }

    /// Two ranges of 4 and 6 bytes, with an empty one between them.
    fn three(a: &mut Mem, b: &mut Mem) -> Vec<CursorRange> {
        vec![a.range(0, 4), CursorRange::default(), b.range(0, 6)]
    }

    fn position(c: &Cursor) -> (u64, u64, u64, u64) {
        (c.range_index, c.range_offset, c.length, c.offset)
    }

    /// The `Debug` form names the position fields (diagnostics only).
    #[test]
    fn debug_names_the_position() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let c = Cursor::with_ranges(three(&mut a, &mut b));
        let s = format!("{c:?}");
        assert!(s.starts_with("Cursor { ranges: ["), "{s}");
        assert!(
            s.contains("range_index: 0, range_offset: 0, length: 10, offset: 0"),
            "{s}"
        );
    }

    #[test]
    fn with_ranges() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let c = Cursor::with_ranges(three(&mut a, &mut b));
        assert_eq!(position(&c), (0, 0, 10, 0));
        assert!(c.keep_alive.is_none());
        // A checked sum: on overflow the length is 0.
        let big = CursorRange {
            address: HostPtr::NULL,
            length: u64::MAX,
        };
        let c = Cursor::with_ranges(vec![big, big]);
        assert_eq!(c.length, 0);
        assert_eq!(Cursor::with_ranges(Vec::new()).length, 0);
    }

    #[test]
    fn with_pointer() {
        let mut a = Mem::new(8, 0);
        let owner: Arc<dyn Any + Send + Sync> = Arc::new(5u32);
        let r = a.range(0, 8);
        let mut c = Cursor::with_pointer(Arc::clone(&owner), r.address, 8);
        // A leading {NULL, 0} range.
        assert_eq!(*c.ranges, vec![CursorRange::default(), r]);
        assert_eq!(position(&c), (0, 0, 8, 0));
        assert_eq!(Arc::strong_count(&owner), 2);
        let mut out = [0u8; 3];
        c.read_bytes(&mut out).unwrap();
        assert_eq!(out, [0, 1, 2]);
        assert_eq!(position(&c), (1, 3, 8, 3));
        let sub = c.sub_cursor(2).unwrap();
        assert_eq!(Arc::strong_count(&owner), 3, "copies share the keep-alive");
        drop(sub);
        drop(c);
        assert_eq!(Arc::strong_count(&owner), 1);
    }

    #[test]
    fn advance() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let mut c = Cursor::with_ranges(three(&mut a, &mut b));
        c.advance(0);
        assert_eq!(position(&c), (0, 0, 10, 0));
        c.advance(4); // to the end of range 0: index 1
        assert_eq!(position(&c), (1, 0, 10, 4));
        c.advance(2); // steps over the empty range
        assert_eq!(position(&c), (2, 2, 10, 6));
        // Exactly to the end saturates without walking.
        c.advance(4);
        assert_eq!(position(&c), (2, 2, 10, 10));
        let mut c = Cursor::with_ranges(three(&mut a, &mut b));
        c.advance(1);
        c.advance(u64::MAX); // overflow saturates too
        assert_eq!(position(&c), (0, 1, 10, 10));
        assert_eq!(c.remaining(), 0);
    }

    #[test]
    fn read_bytes() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let mut c = Cursor::with_ranges(three(&mut a, &mut b));
        let mut out = [0u8; 6];
        c.read_bytes(&mut out).unwrap();
        assert_eq!(out, [0, 1, 2, 3, 100, 101]);
        assert_eq!(position(&c), (2, 2, 10, 6));
        let mut out = [0u8; 5];
        assert_eq!(
            c.read_bytes(&mut out),
            Err("read of 0x5 bytes at offset 0x6 runs past the cursor's 0xa bytes".to_owned())
        );
        assert_eq!(position(&c), (2, 2, 10, 6), "a refused read moves nothing");
        let mut none: [u8; 0] = [];
        c.read_bytes(&mut none).unwrap();
        let mut out = [0u8; 4];
        c.read_bytes(&mut out).unwrap();
        assert_eq!(out, [102, 103, 104, 105]);
        assert_eq!(position(&c), (3, 0, 10, 10));
        // The length past the cursor's alone.
        let mut empty = Cursor::default();
        assert_eq!(
            empty.read_bytes(&mut [0u8; 1]),
            Err("read of 0x1 bytes at offset 0x0 runs past the cursor's 0x0 bytes".to_owned())
        );
    }

    /// A NULL range address faults
    #[test]
    fn read_bytes_from_a_null_range_faults() {
        let mut c = Cursor::with_ranges(vec![CursorRange {
            address: HostPtr::NULL,
            length: 8,
        }]);
        let d = catch_death(|| c.read_bytes(&mut [0u8; 4])).unwrap_err();
        assert_eq!(d, DeathKind::CursorReadNull.site());
        let c = Cursor::with_ranges(vec![CursorRange {
            address: HostPtr::NULL,
            length: 8,
        }]);
        let d = catch_death(|| c.read_bytes_at(2, &mut [0u8; 4])).unwrap_err();
        assert_eq!(d, DeathKind::CursorReadNull.site());
    }

    #[test]
    fn read_bytes_at() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let mut c = Cursor::with_ranges(three(&mut a, &mut b));
        c.advance(1);
        let mut out = [0u8; 3];
        // at 4 from offset 1: skips range 0 (position 5 > 4), then range 1, lands at b[1].
        c.read_bytes_at(4, &mut out).unwrap();
        assert_eq!(out, [101, 102, 103]);
        assert_eq!(position(&c), (0, 1, 10, 1), "the cursor does not move");
        c.read_bytes_at(0, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3]);
        // at landing exactly on a range end copies 0 bytes there and goes on.
        c.read_bytes_at(3, &mut out).unwrap();
        assert_eq!(out, [100, 101, 102]);
        assert_eq!(
            c.read_bytes_at(7, &mut out),
            Err("read of 0x3 bytes at offset 0x1+0x7 runs past the cursor's 0xa bytes".to_owned())
        );
        assert_eq!(
            c.read_bytes_at(u64::MAX, &mut out),
            Err(
                "read of 0x3 bytes at offset 0x1+0xffffffffffffffff runs past the cursor's 0xa bytes"
                    .to_owned()
            )
        );
    }

    #[test]
    fn read_t() {
        let mut a = Mem::new(8, 1);
        let mut c = Cursor::with_ranges(vec![a.range(0, 8)]);
        assert_eq!(
            c.read_at::<u32>(4).unwrap(),
            u32::from_le_bytes([5, 6, 7, 8])
        );
        assert_eq!(c.read::<u16>().unwrap(), u16::from_le_bytes([1, 2]));
        assert_eq!(c.remaining(), 6);
        assert_eq!(
            c.read::<u64>(),
            Err("read of 0x8 bytes at offset 0x2 runs past the cursor's 0x8 bytes".to_owned())
        );
        assert_eq!(
            c.read_at::<u64>(0),
            Err("read of 0x8 bytes at offset 0x2+0x0 runs past the cursor's 0x8 bytes".to_owned())
        );
    }

    #[test]
    fn read_or_raise() {
        let mut a = Mem::new(4, 1);
        let mut c = Cursor::with_ranges(vec![a.range(0, 4)]);
        let e = ({
            assert_eq!(c.read_or_raise::<u16>(OnFault::Return).unwrap(), 0x0201);
            c.read_or_raise::<u32>(OnFault::Return)
        })
        .unwrap_err();
        assert_eq!(e.id(), RaiseId::Memory(RaiseKind::CursorReadFailed));
        assert_eq!(
            e.detail(),
            Some(
                "record not readable: read of 0x4 bytes at offset 0x2 runs past the cursor's 0x4 bytes"
            )
        );
        // With no handler live, the process dies at the raise.
        let d = catch_death(|| c.read_or_raise::<u32>(OnFault::Die)).unwrap_err();
        assert_eq!(
            d,
            crate::raise::death::DeathSite {
                kind: crate::raise::death::DeathKind::UnhandledRaise,
                file: RaiseKind::CursorReadFailed.name(),
                line: 0
            }
        );
    }

    #[test]
    fn sub_cursor() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let mut c = Cursor::with_ranges(three(&mut a, &mut b));
        c.advance(1);
        let mut sub = c.sub_cursor(5).unwrap();
        assert_eq!(position(&sub), (0, 1, 5, 0));
        assert_eq!(position(&c), (2, 2, 10, 6), "the parent advances");
        assert!(Arc::ptr_eq(&sub.ranges, &c.ranges));
        let mut out = [0u8; 5];
        sub.read_bytes(&mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 100, 101]);
        assert_eq!(
            c.sub_cursor(5).unwrap_err(),
            "a sub-cursor of 0x5 bytes, with 0x4 left"
        );
        // To the end: the parent saturates without walking.
        let rest = c.sub_cursor(4).unwrap();
        assert_eq!(position(&rest), (2, 2, 4, 0));
        assert_eq!(position(&c), (2, 2, 10, 10));
    }

    #[test]
    fn sub_cursor_at() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let mut c = Cursor::with_ranges(three(&mut a, &mut b));
        c.advance(1);
        let mut sub = c.sub_cursor_at(4, 3).unwrap();
        assert_eq!(position(&c), (0, 1, 10, 1), "this cursor does not move");
        assert_eq!(position(&sub), (2, 1, 7, 4));
        assert_eq!(sub.remaining(), 3);
        let mut out = [0u8; 3];
        sub.read_bytes(&mut out).unwrap();
        assert_eq!(out, [101, 102, 103]);
        assert_eq!(
            c.sub_cursor_at(5, 5).unwrap_err(),
            "a sub-cursor of 0x5 bytes at 0x5, with 0x9 left"
        );
        assert_eq!(
            c.sub_cursor_at(u64::MAX, 0).unwrap_err(),
            "a sub-cursor of 0x0 bytes at 0xffffffffffffffff overflows"
        );
        assert_eq!(
            c.sub_cursor_at(1, u64::MAX).unwrap_err(),
            "a sub-cursor of 0xffffffffffffffff bytes at 0x1 overflows"
        );
        // A sub-cursor at the very end: advance(at) saturates at its own length.
        let end = c.sub_cursor_at(9, 0).unwrap();
        assert_eq!(position(&end), (0, 1, 9, 9));
        assert_eq!(end.remaining(), 0);
    }

    #[test]
    fn truncate_to() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let mut c = Cursor::with_ranges(three(&mut a, &mut b));
        c.advance(3);
        let copy = c.clone();
        assert_eq!(
            c.truncate_to(11).unwrap_err(),
            "length 0xb is past the cursor's 0xa"
        );
        assert_eq!(
            c.truncate_to(2).unwrap_err(),
            "length 0x2 of a cursor of 0xa is before its offset 0x3"
        );
        c.truncate_to(3).unwrap();
        assert_eq!(c.remaining(), 0);
        assert_eq!(copy.length, 10, "a copy keeps its own length");
    }

    #[test]
    fn remaining() {
        let mut c = Cursor::default();
        assert_eq!(c.remaining(), 0);
        c.offset = 1; // never a real state; the subtraction wraps
        assert_eq!(c.remaining(), u64::MAX);
    }

    #[test]
    fn drop_shares_ranges() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let c = Cursor::with_ranges(three(&mut a, &mut b));
        let d = c.clone();
        assert_eq!(Arc::strong_count(&c.ranges), 2);
        drop(d);
        assert_eq!(Arc::strong_count(&c.ranges), 1);
    }

    #[test]
    fn writable_constructors() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let w = WritableCursor::with_ranges(three(&mut a, &mut b));
        assert_eq!(position(&w.0), (0, 0, 10, 0));
        let r = a.range(0, 4);
        let w = WritableCursor::with_pointer(Arc::new(()), r.address, 4);
        assert_eq!(w.0.ranges.len(), 2);
        assert_eq!(w.0.length, 4);
    }

    #[test]
    fn writable_write_bytes() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let mut w = WritableCursor::with_ranges(three(&mut a, &mut b));
        w.write_bytes(&[9, 8, 7, 6, 5]).unwrap();
        assert_eq!(position(&w.0), (2, 1, 10, 5));
        assert_eq!(
            w.write_bytes(&[0; 6]),
            Err("write of 0x6 bytes at offset 0x5 runs past the cursor's 0xa bytes".to_owned())
        );
        w.write::<u16>(&0x0102).unwrap();
        drop(w);
        assert_eq!(a.0, vec![9, 8, 7, 6]);
        assert_eq!(b.0, vec![5, 2, 1, 103, 104, 105]);
    }

    #[test]
    fn writable_write_bytes_at() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let mut w = WritableCursor::with_ranges(three(&mut a, &mut b));
        w.0.advance(2);
        w.write_bytes_at(1, &[1, 1, 1]).unwrap();
        assert_eq!(position(&w.0), (0, 2, 10, 2), "the cursor does not move");
        w.write_at::<u8>(7, &0xee).unwrap();
        assert_eq!(
            w.write_bytes_at(8, &[0]),
            Err("write of 0x1 bytes at offset 0x2+0x8 runs past the cursor's 0xa bytes".to_owned())
        );
        drop(w);
        assert_eq!(a.0, vec![0, 1, 2, 1]);
        assert_eq!(b.0, vec![1, 1, 102, 103, 104, 0xee]);
    }

    /// Both overloads, into a NULL range
    #[test]
    fn writable_write_t_to_a_null_range_faults() {
        let mut w = WritableCursor::with_ranges(vec![CursorRange {
            address: HostPtr::NULL,
            length: 8,
        }]);
        let d = catch_death(|| w.write::<u32>(&1)).unwrap_err();
        assert_eq!(d, DeathKind::CursorWriteNull.site());
        let d = catch_death(|| w.write_at::<u32>(4, &1)).unwrap_err();
        assert_eq!(d, DeathKind::CursorWriteNull.site());
    }

    #[test]
    fn writable_sub_cursors() {
        let (mut a, mut b) = (Mem::new(4, 0), Mem::new(6, 100));
        let mut w = WritableCursor::with_ranges(three(&mut a, &mut b));
        let mut first = w.writable_sub_cursor(3).unwrap();
        assert_eq!(position(&w.0), (0, 3, 10, 3));
        first.write_bytes(&[7, 7, 7]).unwrap();
        let mut at = w.writable_sub_cursor_at(2, 2).unwrap();
        at.write_bytes(&[9, 9]).unwrap();
        assert_eq!(
            w.writable_sub_cursor(8).unwrap_err(),
            "a sub-cursor of 0x8 bytes, with 0x7 left"
        );
        assert_eq!(
            w.writable_sub_cursor_at(u64::MAX, 1).unwrap_err(),
            "a sub-cursor of 0x1 bytes at 0xffffffffffffffff overflows"
        );
        drop((w, first, at));
        assert_eq!(a.0, vec![7, 7, 7, 3]);
        assert_eq!(b.0, vec![100, 9, 9, 103, 104, 105]);
    }
}
