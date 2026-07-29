//! Byte-capacity LRU cache used for loaded file contents and computed line diffs.
//!
//! Unlike an entry-count cache, this cache evicts least-recently-used entries
//! until their combined estimated memory use fits within `capacity`. A single
//! newly inserted entry may exceed the capacity: keeping it allows a large file
//! to remain usable while older entries are still evicted.

use std::collections::HashMap;
use std::hash::Hash;

struct Entry<V> {
    value: V,
    weight: usize,
    last_used: u64,
}

/// Least-recently-used cache whose limit is expressed in estimated bytes.
///
/// The caller supplies each entry's weight because `size_of::<V>()` does not
/// include heap allocations owned by `V`.
pub struct WeightedLru<K, V> {
    entries: HashMap<K, Entry<V>>,
    capacity: usize,
    total_weight: usize,
    clock: u64,
}

impl<K: Eq + Hash + Clone, V> WeightedLru<K, V> {
    /// Creates an empty cache with the given byte budget.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            total_weight: 0,
            clock: 0,
        }
    }

    /// Inserts or replaces an entry and marks it as most recently used.
    ///
    /// Entries are evicted immediately when the total exceeds the budget. The
    /// newly inserted entry itself is retained even when it alone is larger
    /// than the budget.
    pub fn insert(&mut self, key: K, value: V, weight: usize) {
        self.clock = self.clock.wrapping_add(1);
        if let Some(previous) = self.entries.remove(&key) {
            self.total_weight = self.total_weight.saturating_sub(previous.weight);
        }
        self.total_weight = self.total_weight.saturating_add(weight);
        self.entries.insert(
            key.clone(),
            Entry {
                value,
                weight,
                last_used: self.clock,
            },
        );

        // Retain one oversized new entry so callers can display a large file.
        // Everything older is still eligible for eviction.
        while self.total_weight > self.capacity && self.entries.len() > 1 {
            let victim = self
                .entries
                .iter()
                .filter(|(candidate, _)| *candidate != &key)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(candidate, _)| candidate.clone());
            let Some(victim) = victim else {
                break;
            };
            if let Some(removed) = self.entries.remove(&victim) {
                self.total_weight = self.total_weight.saturating_sub(removed.weight);
            }
        }
    }

    /// Returns a cloned value and marks the entry as most recently used.
    pub fn get_cloned(&mut self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.clock;
        Some(entry.value.clone())
    }

    /// Returns whether the key is present without changing recency.
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Returns the combined caller-supplied weight of all retained entries.
    pub fn total_weight(&self) -> usize {
        self.total_weight
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_by_weight() {
        let mut cache = WeightedLru::new(10);
        cache.insert("a", 1, 4);
        cache.insert("b", 2, 4);
        assert_eq!(cache.get_cloned(&"a"), Some(1));
        cache.insert("c", 3, 4);
        assert!(cache.contains_key(&"a"));
        assert!(!cache.contains_key(&"b"));
        assert!(cache.contains_key(&"c"));
        assert_eq!(cache.total_weight(), 8);
    }

    #[test]
    fn keeps_one_oversized_item() {
        let mut cache = WeightedLru::new(10);
        cache.insert("huge", 1, 20);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.total_weight(), 20);
    }
}
