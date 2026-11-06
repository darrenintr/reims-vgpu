//! [`ListOrderMap`]: a `u32`-keyed hash map that can be walked and drained.
//!
//! The resource caches are these maps. A flush walks one to sort its resources into evictions,
//! and a teardown drains each kind's objects to release them. Nothing the guest or host sees
//! depends on that order. The hasher is fixed ([`super::KeyHasher`]), so the order is a function
//! of the keys and the insert and remove history, the same in every run.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use super::hasher::KeyBuild;

/// A map from `u32` keys, walked and drained in its hash table's order.
#[derive(Debug)]
pub struct ListOrderMap<V> {
    map: HashMap<u32, V, KeyBuild>,
}

impl<V> Default for ListOrderMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> ListOrderMap<V> {
    /// An empty map.
    pub fn new() -> Self {
        ListOrderMap {
            map: HashMap::with_hasher(KeyBuild::default()),
        }
    }

    /// The value under `key`.
    pub fn get(&self, key: u32) -> Option<&V> {
        self.map.get(&key)
    }

    /// Inserts `v` under `key` when the key is absent. A present key keeps its value, and `v`
    /// comes back.
    pub fn try_insert(&mut self, key: u32, v: V) -> Result<(), V> {
        match self.map.entry(key) {
            Entry::Occupied(_) => Err(v),
            Entry::Vacant(e) => {
                e.insert(v);
                Ok(())
            }
        }
    }

    /// Removes `key` and returns its value.
    pub fn remove(&mut self, key: u32) -> Option<V> {
        self.map.remove(&key)
    }

    /// Removes every key and returns the values in [`iter`](Self::iter)'s order, so that the
    /// caller decides where they drop.
    pub fn drain(&mut self) -> Vec<V> {
        self.map.drain().map(|(_, v)| v).collect()
    }

    /// The keys and values.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &V)> {
        self.map.iter().map(|(k, v)| (*k, v))
    }

    /// Whether the map holds no key.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_of(ks: &[u32]) -> ListOrderMap<u32> {
        let mut m = ListOrderMap::new();
        for &k in ks {
            assert!(m.try_insert(k, k.wrapping_mul(10)).is_ok());
        }
        m
    }

    #[test]
    fn looks_up_inserts_and_removes() {
        let mut m = map_of(&[]);
        assert!(m.is_empty());
        assert_eq!(m.get(0), None);
        assert_eq!(m.remove(3), None);
        let d: ListOrderMap<u8> = ListOrderMap::default();
        assert!(d.is_empty());
        m.try_insert(3, 30).unwrap();
        assert_eq!(m.get(3), Some(&30));
        assert!(!m.is_empty());
        assert_eq!(m.remove(3), Some(30));
        assert_eq!(m.remove(3), None);
        assert!(m.is_empty());
    }

    #[test]
    fn try_insert_keeps_the_existing_value() {
        let mut m = map_of(&[4]);
        assert_eq!(m.try_insert(4, 99), Err(99));
        assert_eq!(m.get(4), Some(&40));
        assert_eq!(m.iter().count(), 1);
    }

    #[test]
    fn iter_yields_every_key_once() {
        let ks = [7u32, 1, 12, 3, 30, 0, u32::MAX];
        let m = map_of(&ks);
        let mut seen: Vec<(u32, u32)> = m.iter().map(|(k, v)| (k, *v)).collect();
        seen.sort_unstable();
        let mut want: Vec<(u32, u32)> = ks.iter().map(|&k| (k, k.wrapping_mul(10))).collect();
        want.sort_unstable();
        assert_eq!(seen, want);
    }

    #[test]
    fn drain_yields_iteration_order_and_empties() {
        let mut m = map_of(&[7, 1, 12, 3, 30]);
        let order: Vec<u32> = m.iter().map(|(_, v)| *v).collect();
        assert_eq!(m.drain(), order);
        assert!(m.is_empty());
        assert_eq!(m.drain(), Vec::<u32>::new());
        m.try_insert(1, 1).unwrap();
        assert_eq!(m.get(1), Some(&1));
    }

    /// The same history walks in the same order.
    #[test]
    fn the_order_is_a_function_of_the_history() {
        let walk = || {
            let mut m = map_of(&[9, 2, 14, 3, 25, 0, 40, 41]);
            m.remove(14);
            m.try_insert(14, 0).unwrap();
            m.iter().map(|(k, _)| k).collect::<Vec<_>>()
        };
        assert_eq!(walk(), walk());
    }
}
