//! An in-memory key-value store for query results.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use violet_core::ConnectionId;
use violet_core::ports::{CacheKey, CachedOutput, ResultCache};

/// A bounded key-value map that drops the least recently used entries
/// once their total weight passes a budget.
///
/// Eviction scans for the oldest entry. Caches here hold tens to hundreds
/// of results, where a scan costs less than keeping a linked order.
#[derive(Debug)]
pub struct MemoryKv<K, V> {
    entries: HashMap<K, Entry<V>>,
    budget: usize,
    weight: usize,
    clock: u64,
}

#[derive(Debug)]
struct Entry<V> {
    value: V,
    weight: usize,
    used: u64,
}

impl<K: Eq + Hash + Clone, V: Clone> MemoryKv<K, V> {
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget,
            weight: 0,
            clock: 0,
        }
    }

    pub fn get(&mut self, key: &K) -> Option<V> {
        self.clock += 1;
        let entry = self.entries.get_mut(key)?;
        entry.used = self.clock;
        Some(entry.value.clone())
    }

    /// Stores `value`, evicting older entries to stay within the budget. A
    /// value heavier than the whole budget is not kept.
    pub fn put(&mut self, key: K, value: V, weight: usize) {
        self.remove(&key);
        if weight > self.budget {
            return;
        }
        while self.weight + weight > self.budget {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
        self.clock += 1;
        self.weight += weight;
        self.entries.insert(
            key,
            Entry {
                value,
                weight,
                used: self.clock,
            },
        );
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let entry = self.entries.remove(key)?;
        self.weight -= entry.weight;
        Some(entry.value)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let weight = &mut self.weight;
        self.entries.retain(|key, entry| {
            let kept = keep(key, &entry.value);
            if !kept {
                *weight -= entry.weight;
            }
            kept
        });
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The total weight held.
    #[must_use]
    pub fn weight(&self) -> usize {
        self.weight
    }
}

/// Query results kept in memory only: they may hold anything the user can
/// read, so they never reach the disk, and they go when the app closes.
#[derive(Debug)]
pub struct MemoryResultCache {
    store: Mutex<MemoryKv<CacheKey, CachedOutput>>,
    ttl: Duration,
}

impl MemoryResultCache {
    /// 64 MiB of results, each trusted for at most five minutes.
    pub const DEFAULT_BUDGET: usize = 64 * 1024 * 1024;
    pub const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

    #[must_use]
    pub fn new(budget: usize, ttl: Duration) -> Self {
        Self {
            store: Mutex::new(MemoryKv::new(budget)),
            ttl,
        }
    }

    fn store(&self) -> MutexGuard<'_, MemoryKv<CacheKey, CachedOutput>> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for MemoryResultCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_BUDGET, Self::DEFAULT_TTL)
    }
}

/// Roughly the heap a cached result holds.
fn weight(key: &CacheKey, value: &CachedOutput) -> usize {
    let sets: usize = value
        .output
        .sets
        .iter()
        .map(|set| {
            set.rows.heap_size()
                + set
                    .columns
                    .iter()
                    .map(|c| c.name.len() + c.type_name.as_ref().map_or(0, String::len) + 64)
                    .sum::<usize>()
                + 128
        })
        .sum();
    key.sql.len() + value.token.as_ref().map_or(0, String::len) + sets + 256
}

impl ResultCache for MemoryResultCache {
    #[hotpath::measure]
    fn get(&self, key: &CacheKey) -> Option<CachedOutput> {
        let mut store = self.store();
        let value = store.get(key)?;
        if value.stored.elapsed() > self.ttl {
            store.remove(key);
            return None;
        }
        Some(value)
    }

    #[hotpath::measure]
    fn put(&self, key: CacheKey, value: CachedOutput) {
        let weight = weight(&key, &value);
        self.store().put(key, value, weight);
    }

    fn invalidate(&self, connection: ConnectionId) {
        self.store().retain(|key, _| key.connection != connection);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use violet_core::{QueryOutput, ResultSet};

    use super::*;

    #[test]
    fn least_recently_used_entries_go_first() {
        let mut kv = MemoryKv::new(10);
        kv.put("a", 1, 4);
        kv.put("b", 2, 4);
        assert_eq!(kv.get(&"a"), Some(1));
        kv.put("c", 3, 4);
        assert_eq!(kv.get(&"b"), None, "b was the least recently used");
        assert_eq!(kv.get(&"a"), Some(1));
        assert_eq!(kv.get(&"c"), Some(3));
        assert_eq!(kv.weight(), 8);

        // Replacing a key frees its old weight; oversized values are dropped.
        kv.put("a", 9, 2);
        assert_eq!(kv.weight(), 6);
        kv.put("huge", 0, 11);
        assert_eq!(kv.get(&"huge"), None);
        assert_eq!(kv.len(), 2);

        kv.retain(|key, _| *key != "a");
        assert_eq!((kv.len(), kv.weight()), (1, 4));
    }

    fn output(stored: Instant) -> CachedOutput {
        let mut set = ResultSet::default();
        set.rows.push([Some("1")]);
        CachedOutput {
            output: Arc::new(QueryOutput {
                sets: vec![set],
                ..QueryOutput::default()
            }),
            token: Some("t".into()),
            stored,
        }
    }

    #[test]
    fn results_expire_and_are_dropped_per_connection() {
        let cache = MemoryResultCache::new(1 << 20, Duration::from_secs(60));
        let mine = ConnectionId::new();
        let other = ConnectionId::new();
        let key = |connection, sql: &str| CacheKey {
            connection,
            sql: sql.into(),
        };

        cache.put(key(mine, "SELECT 1"), output(Instant::now()));
        cache.put(key(other, "SELECT 1"), output(Instant::now()));
        assert!(cache.get(&key(mine, "SELECT 1")).is_some());
        assert!(cache.get(&key(mine, "SELECT 2")).is_none());

        if let Some(old) = Instant::now().checked_sub(Duration::from_secs(61)) {
            cache.put(key(mine, "SELECT old"), output(old));
            assert!(cache.get(&key(mine, "SELECT old")).is_none(), "expired");
        }

        cache.invalidate(mine);
        assert!(cache.get(&key(mine, "SELECT 1")).is_none());
        assert!(cache.get(&key(other, "SELECT 1")).is_some());
    }
}
