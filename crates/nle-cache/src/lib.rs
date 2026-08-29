//! Bounded decoded-frame cache for timeline monitoring.
//!
//! This cache is deliberately independent of decode and rendering. Callers
//! decide when a decoded frame is useful; the cache only retains exact source
//! frame keys within its byte budget.

use std::{collections::HashMap, sync::Arc};

/// Identifies one decoded source frame at one requested output size.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FrameKey {
    pub project_epoch: u64,
    pub media_id: u32,
    pub source_tick: i64,
    pub width: u32,
    pub height: u32,
}

/// A decoded RGBA frame retained by [`FrameCache`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameValue {
    pub source_tick: i64,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

impl FrameValue {
    pub fn new(source_tick: i64, width: u32, height: u32, rgba: Arc<[u8]>) -> Self {
        Self {
            source_tick,
            width,
            height,
            rgba,
        }
    }

    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

#[derive(Debug)]
struct Entry {
    value: FrameValue,
    last_access: u64,
}

/// Exact-key, byte-bounded LRU cache for decoded monitor frames.
#[derive(Debug)]
pub struct FrameCache {
    capacity_bytes: usize,
    used_bytes: usize,
    next_access: u64,
    entries: HashMap<FrameKey, Entry>,
}

impl FrameCache {
    pub fn new(capacity_bytes: usize) -> Self {
        Self {
            capacity_bytes,
            used_bytes: 0,
            next_access: 0,
            entries: HashMap::new(),
        }
    }

    pub fn capacity_bytes(&self) -> usize {
        self.capacity_bytes
    }

    pub fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns an exact cached frame and marks it most recently used.
    pub fn get(&mut self, key: &FrameKey) -> Option<&FrameValue> {
        let access = self.take_access_generation();
        let entry = self.entries.get_mut(key)?;
        entry.last_access = access;
        Some(&entry.value)
    }

    /// Inserts or replaces a frame. Returns `false` without changing the cache
    /// when the value cannot ever fit within this cache's hard byte budget.
    pub fn insert(&mut self, key: FrameKey, value: FrameValue) -> bool {
        let value_bytes = value.byte_len();
        if value_bytes > self.capacity_bytes {
            return false;
        }

        if let Some(previous) = self.entries.remove(&key) {
            self.used_bytes -= previous.value.byte_len();
        }

        let access = self.take_access_generation();
        self.used_bytes = self.used_bytes.saturating_add(value_bytes);
        self.entries.insert(
            key,
            Entry {
                value,
                last_access: access,
            },
        );
        self.evict_to_budget();
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.used_bytes = 0;
    }

    /// Removes one exact frame and returns it without disturbing other recency state.
    pub fn remove(&mut self, key: &FrameKey) -> Option<FrameValue> {
        let removed = self.entries.remove(key)?;
        self.used_bytes -= removed.value.byte_len();
        Some(removed.value)
    }

    fn take_access_generation(&mut self) -> u64 {
        if self.next_access == u64::MAX {
            self.renumber_access_generations();
        }
        self.next_access += 1;
        self.next_access
    }

    /// Preserves LRU ordering before a counter rollover. FrameKey makes the
    /// otherwise-impossible tied ordering deterministic too.
    fn renumber_access_generations(&mut self) {
        let mut ordered: Vec<(FrameKey, u64)> = self
            .entries
            .iter()
            .map(|(key, entry)| (*key, entry.last_access))
            .collect();
        ordered.sort_unstable_by_key(|(key, access)| (*access, *key));
        for (index, (key, _)) in ordered.into_iter().enumerate() {
            self.entries
                .get_mut(&key)
                .expect("entry collected from cache must remain present")
                .last_access = index as u64 + 1;
        }
        self.next_access = self.entries.len() as u64;
    }

    fn evict_to_budget(&mut self) {
        while self.used_bytes > self.capacity_bytes {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(key, entry)| (entry.last_access, **key))
                .map(|(key, _)| *key)
            else {
                break;
            };
            let evicted = self
                .entries
                .remove(&oldest_key)
                .expect("oldest key was selected from cache");
            self.used_bytes -= evicted.value.byte_len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source_tick: i64) -> FrameKey {
        FrameKey {
            project_epoch: 7,
            media_id: 11,
            source_tick,
            width: 4,
            height: 4,
        }
    }

    fn value(source_tick: i64, bytes: usize) -> FrameValue {
        FrameValue::new(source_tick, 4, 4, vec![source_tick as u8; bytes].into())
    }

    #[test]
    fn exact_key_hit_and_miss() {
        let mut cache = FrameCache::new(128);
        let frame_key = key(10);
        assert!(cache.insert(frame_key, value(10, 16)));
        assert_eq!(cache.get(&frame_key).unwrap().source_tick, 10);
        assert!(cache.get(&key(11)).is_none());
    }

    #[test]
    fn get_refreshes_lru_recency_before_eviction() {
        let mut cache = FrameCache::new(20);
        assert!(cache.insert(key(1), value(1, 10)));
        assert!(cache.insert(key(2), value(2, 10)));
        assert!(cache.get(&key(1)).is_some());
        assert!(cache.insert(key(3), value(3, 10)));
        assert!(cache.get(&key(1)).is_some());
        assert!(cache.get(&key(2)).is_none());
        assert!(cache.get(&key(3)).is_some());
    }

    #[test]
    fn replacement_updates_byte_accounting() {
        let mut cache = FrameCache::new(64);
        assert!(cache.insert(key(1), value(1, 12)));
        assert_eq!(cache.used_bytes(), 12);
        assert!(cache.insert(key(1), value(1, 20)));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used_bytes(), 20);
    }

    #[test]
    fn sustained_sixty_second_scrub_never_exceeds_hard_cap() {
        let mut cache = FrameCache::new(2_048);
        for tick in 0..(60 * 60) {
            assert!(cache.insert(key(tick), value(tick, 128)));
            assert!(cache.used_bytes() <= cache.capacity_bytes());
        }
        assert_eq!(cache.used_bytes(), 2_048);
        assert_eq!(cache.len(), 16);
        assert!(cache.get(&key(0)).is_none());
        assert!(cache.get(&key(3_599)).is_some());
    }

    #[test]
    fn oversize_value_is_rejected_without_displacing_existing_frame() {
        let mut cache = FrameCache::new(10);
        assert!(cache.insert(key(1), value(1, 10)));
        assert!(!cache.insert(key(2), value(2, 11)));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used_bytes(), 10);
        assert!(cache.get(&key(1)).is_some());
    }

    #[test]
    fn clear_releases_all_accounted_bytes() {
        let mut cache = FrameCache::new(64);
        assert!(cache.insert(key(1), value(1, 16)));
        assert!(cache.insert(key(2), value(2, 16)));
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.used_bytes(), 0);
    }

    #[test]
    fn remove_updates_byte_accounting() {
        let mut cache = FrameCache::new(64);
        assert!(cache.insert(key(1), value(1, 16)));
        assert_eq!(cache.remove(&key(1)).unwrap().byte_len(), 16);
        assert!(cache.is_empty());
        assert_eq!(cache.used_bytes(), 0);
        assert!(cache.remove(&key(1)).is_none());
    }
}
