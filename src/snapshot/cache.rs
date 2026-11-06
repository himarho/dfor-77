use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::memory::HeapSize;

struct Entry<T> {
    key: String,
    value: T,
    expires_at: Option<u64>,
    rest_guard_at_ms: Option<u64>,
}

pub(super) struct SnapshotCache<T> {
    index: HashMap<String, u64>,
    entries: BTreeMap<u64, Entry<T>>,
    expiry: BTreeSet<(u64, u64)>,
    next_id: u64,
    bytes: usize,
    max_entry_bytes: usize,
    rest_guard_lost: bool,
}

impl<T> std::fmt::Debug for SnapshotCache<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnapshotCache")
            .field("entries", &self.entries.len())
            .field("expiring_entries", &self.expiry.len())
            .field("bytes", &self.bytes)
            .field("max_entry_bytes", &self.max_entry_bytes)
            .finish()
    }
}

impl<T> Default for SnapshotCache<T> {
    fn default() -> Self {
        Self {
            index: HashMap::new(),
            entries: BTreeMap::new(),
            expiry: BTreeSet::new(),
            next_id: 0,
            bytes: 0,
            max_entry_bytes: usize::MAX,
            rest_guard_lost: false,
        }
    }
}

impl<T: HeapSize + Clone> Clone for SnapshotCache<T> {
    fn clone(&self) -> Self {
        let mut copy = Self::with_max_entry_bytes(self.max_entry_bytes);
        for entry in self.entries.values() {
            copy.insert_expiring(entry.key.clone(), entry.value.clone(), entry.expires_at);
            if let Some(at_ms) = entry.rest_guard_at_ms {
                copy.mark_rest_guard(&entry.key, at_ms);
            }
        }
        copy.rest_guard_lost = self.rest_guard_lost;
        copy
    }
}

impl<T: HeapSize> SnapshotCache<T> {
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }
    pub(super) fn contains_key(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }
    pub(super) fn get(&self, key: &str) -> Option<&T> {
        self.entries
            .get(self.index.get(key)?)
            .map(|entry| &entry.value)
    }
    pub(super) fn values(&self) -> impl Iterator<Item = &T> {
        self.entries.values().map(|entry| &entry.value)
    }

    pub(super) fn clear(&mut self) {
        let lost = self.rest_guard_lost
            || self
                .entries
                .values()
                .any(|entry| entry.rest_guard_at_ms.is_some());
        *self = Self::with_max_entry_bytes(self.max_entry_bytes);
        self.rest_guard_lost = lost;
    }

    pub(super) fn rest_guard_at(&self, key: &str) -> Option<u64> {
        self.entries
            .get(self.index.get(key)?)
            .and_then(|entry| entry.rest_guard_at_ms)
    }

    pub(super) fn rest_protected(&self, key: &str) -> bool {
        self.rest_guard_lost || self.rest_guard_at(key).is_some()
    }

    pub(super) fn mark_rest_guard(&mut self, key: &str, at_ms: u64) {
        if let Some(entry) = self.index.get(key).and_then(|id| self.entries.get_mut(id)) {
            entry.rest_guard_at_ms = Some(at_ms);
        }
    }

    pub(super) fn require_rest_revalidation(&mut self) {
        self.rest_guard_lost = true;
    }

    pub(super) fn with_max_entry_bytes(max_entry_bytes: usize) -> Self {
        Self {
            max_entry_bytes,
            ..Self::default()
        }
    }

    pub(super) fn set_max_entry_bytes(&mut self, max_entry_bytes: usize) {
        self.max_entry_bytes = max_entry_bytes;
        let keys = self
            .entries
            .values()
            .filter(|entry| entry_bytes(entry) > max_entry_bytes)
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            self.remove(&key);
        }
    }

    pub(super) fn insert(&mut self, key: String, value: T) -> Option<T> {
        self.insert_expiring(key, value, None)
    }

    pub(super) fn insert_expiring(
        &mut self,
        key: String,
        value: T,
        expires_at: Option<u64>,
    ) -> Option<T> {
        let existing_id = self.index.get(&key).copied();
        let rest_guard_at_ms = self.rest_guard_at(&key);
        let prior_guard_lost = self.rest_guard_lost;
        let old = self.remove(&key);
        let id = existing_id.unwrap_or_else(|| {
            while self.entries.contains_key(&self.next_id) {
                self.next_id = self.next_id.wrapping_add(1);
            }
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            id
        });
        let entry = Entry {
            key,
            value,
            expires_at,
            rest_guard_at_ms,
        };
        if entry_bytes(&entry) > self.max_entry_bytes {
            return old;
        }
        self.bytes = self.bytes.saturating_add(entry_bytes(&entry));
        self.index.insert(entry.key.clone(), id);
        if let Some(expires_at) = expires_at {
            self.expiry.insert((expires_at, id));
        }
        self.entries.insert(id, entry);
        self.rest_guard_lost = prior_guard_lost;
        old
    }

    pub(super) fn update<R>(&mut self, key: &str, change: impl FnOnce(&mut T) -> R) -> Option<R> {
        let entry = self.entries.get_mut(self.index.get(key)?)?;
        let before = entry_bytes(entry);
        let result = change(&mut entry.value);
        self.bytes = self
            .bytes
            .saturating_sub(before)
            .saturating_add(entry_bytes(entry));
        if entry_bytes(entry) > self.max_entry_bytes {
            self.remove(key);
        }
        Some(result)
    }

    pub(super) fn for_each_mut(&mut self, mut change: impl FnMut(&mut T)) {
        for entry in self.entries.values_mut() {
            let before = entry_bytes(entry);
            change(&mut entry.value);
            self.bytes = self
                .bytes
                .saturating_sub(before)
                .saturating_add(entry_bytes(entry));
        }
        self.set_max_entry_bytes(self.max_entry_bytes);
    }

    pub(super) fn remove(&mut self, key: &str) -> Option<T> {
        let id = self.index.remove(key)?;
        self.remove_id(id).map(|(_, value)| value)
    }

    fn remove_id(&mut self, id: u64) -> Option<(String, T)> {
        let entry = self.entries.remove(&id)?;
        self.rest_guard_lost |= entry.rest_guard_at_ms.is_some();
        self.index.remove(&entry.key);
        if let Some(expires_at) = entry.expires_at {
            self.expiry.remove(&(expires_at, id));
        }
        self.bytes = self.bytes.saturating_sub(entry_bytes(&entry));
        self.compact_index();
        Some((entry.key, entry.value))
    }

    pub(super) fn pop_oldest(&mut self) -> Option<(String, T)> {
        let id = *self.entries.first_key_value()?.0;
        self.remove_id(id)
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&str, &T) -> bool) {
        let Self {
            entries,
            index,
            expiry,
            bytes,
            rest_guard_lost,
            ..
        } = self;
        entries.retain(|id, entry| {
            if keep(&entry.key, &entry.value) {
                return true;
            }
            index.remove(&entry.key);
            if let Some(expires_at) = entry.expires_at {
                expiry.remove(&(expires_at, *id));
            }
            *bytes = bytes.saturating_sub(entry_bytes(entry));
            *rest_guard_lost |= entry.rest_guard_at_ms.is_some();
            false
        });
        self.compact_index();
    }

    fn compact_index(&mut self) {
        if self.index.is_empty() {
            self.index = HashMap::new();
        } else if self.index.capacity() > self.index.len().saturating_mul(4).max(8) {
            self.index.shrink_to_fit();
        }
    }

    pub(super) fn expire_before(&mut self, now_ms: u64) {
        while let Some((deadline, id)) = self.expiry.first().copied() {
            if deadline >= now_ms {
                break;
            }
            self.expiry.pop_first();
            self.remove_id(id);
        }
    }
}

fn entry_bytes<T: HeapSize>(entry: &Entry<T>) -> usize {
    std::mem::size_of::<Entry<T>>()
        .saturating_add(192)
        .saturating_add(entry.key.capacity().saturating_mul(2))
        .saturating_add(entry.value.heap_size())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consistent(cache: &SnapshotCache<String>) {
        assert_eq!(cache.index.len(), cache.entries.len());
        if cache.entries.is_empty() {
            assert_eq!(cache.index.capacity(), 0);
        }
        assert_eq!(
            cache.bytes(),
            cache.entries.values().map(entry_bytes).sum::<usize>()
        );
        assert_eq!(
            cache.expiry.len(),
            cache
                .entries
                .values()
                .filter(|entry| entry.expires_at.is_some())
                .count()
        );
    }

    #[test]
    fn rest_guards_survive_replacements_clones_and_every_removal_path() {
        let mut source = SnapshotCache::<String>::default();
        source.insert_expiring("member".into(), "old".into(), Some(10));
        source.mark_rest_guard("member", 3);
        source.insert_expiring("member".into(), "same facts".into(), Some(10));
        assert_eq!(source.rest_guard_at("member"), Some(3));
        assert!(!source.rest_protected("unrelated"));
        for removal in 0..6 {
            let mut copy = source.clone();
            assert_eq!(copy.rest_guard_at("member"), Some(3));
            match removal {
                0 => {
                    copy.remove("member");
                }
                1 => {
                    copy.pop_oldest();
                }
                2 => copy.retain(|_, _| false),
                3 => copy.set_max_entry_bytes(1),
                4 => copy.expire_before(11),
                5 => copy.clear(),
                _ => unreachable!(),
            }
            consistent(&copy);
            assert_eq!(copy.len(), 0);
            assert!(copy.rest_protected("new member"), "{removal}");
        }
    }

    #[test]
    fn updates_removals_and_reinsertions_preserve_fifo_without_stale_order_entries() {
        let mut cache = SnapshotCache::<String>::default();
        cache.insert("first".into(), "one".into());
        cache.insert("second".into(), "two".into());
        cache.insert("first".into(), "updated".into());
        assert_eq!(
            cache.values().cloned().collect::<Vec<_>>(),
            ["updated", "two"]
        );
        cache.remove("first");
        cache.insert("first".into(), "new".into());
        assert_eq!(cache.pop_oldest().unwrap().0, "second");
        assert_eq!(cache.pop_oldest().unwrap(), ("first".into(), "new".into()));
        consistent(&cache);
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn expiry_is_exact_with_refreshes_and_out_of_order_timestamps() {
        let mut cache = SnapshotCache::<String>::default();
        cache.insert_expiring("first".into(), "one".into(), Some(100));
        cache.insert_expiring("second".into(), "two".into(), Some(50));
        cache.insert_expiring("first".into(), "renewed".into(), Some(200));
        cache.expire_before(50);
        assert_eq!(cache.len(), 2);
        cache.expire_before(51);
        assert!(!cache.contains_key("second"));
        cache.expire_before(101);
        assert_eq!(cache.get("first").map(String::as_str), Some("renewed"));
        consistent(&cache);
        cache.expire_before(201);
        consistent(&cache);
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn repeated_refresh_and_mutation_keep_both_indexes_and_accounting_bounded() {
        let mut cache = SnapshotCache::<String>::default();
        for iteration in 0..10_000 {
            cache.insert_expiring("only".into(), "text".into(), Some(iteration));
        }
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.expiry.len(), 1);
        let before = cache.bytes();
        cache.update("only", |value| value.reserve(10_000));
        assert!(cache.bytes() >= before + 10_000);
        consistent(&cache);
        cache.for_each_mut(|value| *value = "small".into());
        consistent(&cache);
        cache.retain(|_, _| false);
        consistent(&cache);
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn clones_recalculate_actual_capacities_and_release_all_accounting_on_clear() {
        let mut cache = SnapshotCache::<String>::default();
        let mut value = String::with_capacity(10_000);
        value.push('x');
        cache.insert("value".into(), value);
        let mut clone = cache.clone();
        consistent(&clone);
        assert!(clone.bytes() <= cache.bytes());
        clone.clear();
        consistent(&clone);
        assert_eq!(clone.bytes(), 0);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn deterministic_mutations_match_an_independent_fifo_model() {
        let mut cache = SnapshotCache::<String>::default();
        let mut model = Vec::<(String, String)>::new();
        let mut seed = 17_u64;
        for _ in 0..5000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = format!("key-{}", (seed >> 32) % 32);
            match seed % 4 {
                0 | 1 => {
                    let value = seed.to_string();
                    cache.insert(key.clone(), value.clone());
                    if let Some(existing) = model.iter_mut().find(|entry| entry.0 == key) {
                        existing.1 = value;
                    } else {
                        model.push((key, value));
                    }
                }
                2 => {
                    cache.remove(&key);
                    model.retain(|entry| entry.0 != key);
                }
                _ => {
                    let expected = if model.is_empty() {
                        None
                    } else {
                        Some(model.remove(0))
                    };
                    assert_eq!(cache.pop_oldest(), expected);
                }
            }
            consistent(&cache);
            assert_eq!(
                cache.values().cloned().collect::<Vec<_>>(),
                model
                    .iter()
                    .map(|entry| entry.1.clone())
                    .collect::<Vec<_>>()
            );
        }
    }
}
