//! [`ShortList`]: a record's list of objects or values, held on the stack while it is short.
//!
//! A list record (buffers, textures, samplers, with their offsets) names a few entries almost
//! always, and the decoders make one or more lists for each such record, which every draw of a
//! busy pass sends. Up to [`INLINE`] entries take no allocation; a longer list is a `Vec`. It
//! derefs to a slice, as the sends take it.

use std::ops::Deref;

/// How many entries a list holds without an allocation.
pub(crate) const INLINE: usize = 8;

/// A list of up to [`INLINE`] entries in place, or a longer one on the heap.
pub(crate) enum ShortList<T> {
    /// The entries in place: the array, of which the first `usize` are the list.
    Inline([T; INLINE], usize),
    /// A list longer than [`INLINE`].
    Heap(Vec<T>),
}

impl<T: Default> ShortList<T> {
    /// An empty list with room for `capacity` entries.
    pub(crate) fn with_capacity(capacity: usize) -> ShortList<T> {
        if capacity <= INLINE {
            ShortList::Inline(std::array::from_fn(|_| T::default()), 0)
        } else {
            ShortList::Heap(Vec::with_capacity(capacity))
        }
    }

    /// Adds `value` at the end; a list that outgrows the array moves to the heap.
    pub(crate) fn push(&mut self, value: T) {
        match self {
            ShortList::Inline(array, len) => match array.get_mut(*len) {
                Some(slot) => {
                    *slot = value;
                    *len = len.wrapping_add(1);
                }
                None => {
                    let moved = std::mem::replace(array, std::array::from_fn(|_| T::default()));
                    let mut heap = Vec::with_capacity(INLINE.wrapping_mul(2));
                    heap.extend(moved);
                    heap.push(value);
                    *self = ShortList::Heap(heap);
                }
            },
            ShortList::Heap(heap) => heap.push(value),
        }
    }
}

impl<T: Default> FromIterator<T> for ShortList<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> ShortList<T> {
        let iter = iter.into_iter();
        let mut list = ShortList::with_capacity(iter.size_hint().0);
        iter.for_each(|value| list.push(value));
        list
    }
}

impl<T> Deref for ShortList<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match self {
            ShortList::Inline(array, len) => array.get(..*len).unwrap_or_default(),
            ShortList::Heap(heap) => heap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list holds what was pushed, in order, at every length around the array's, short or
    /// sized past it, and from an iterator.
    #[test]
    fn holds_what_was_pushed() {
        for capacity in [0, 1, INLINE, INLINE + 1] {
            for len in 0..INLINE * 3 {
                let mut list = ShortList::with_capacity(capacity);
                (0..len).for_each(|i| list.push(Some(i)));
                let want: Vec<_> = (0..len).map(Some).collect();
                assert_eq!(&*list, &want[..], "capacity {capacity}, length {len}");
                let collected: ShortList<_> = (0..len).map(Some).collect();
                assert_eq!(&*collected, &want[..]);
            }
        }
    }
}
