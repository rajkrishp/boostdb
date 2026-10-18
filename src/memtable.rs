//! In-memory sorted table holding the most recent writes.
//!
//! Deletes are stored as tombstones (`None`) rather than removing the key, so that once
//! the memtable is flushed to disk the delete can shadow older on-disk values.

use std::collections::BTreeMap;
use std::ops::Bound;

#[derive(Default)]
pub struct MemTable {
    entries: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    approximate_size: usize,
}

impl MemTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.insert(key, Some(value));
    }

    pub fn delete(&mut self, key: Vec<u8>) {
        self.insert(key, None);
    }

    fn insert(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) {
        let added = key.len() + value.as_ref().map_or(0, Vec::len);
        let key_len = key.len();
        if let Some(old) = self.entries.insert(key, value) {
            self.approximate_size -= key_len + old.map_or(0, |v| v.len());
        }
        self.approximate_size += added;
    }

    /// `None` if the key has never been written here, `Some(None)` if it was deleted,
    /// `Some(Some(value))` if it is live.
    pub fn get(&self, key: &[u8]) -> Option<Option<&[u8]>> {
        self.entries.get(key).map(Option::as_deref)
    }

    /// Entries (including tombstones) with keys in `[start, end)`, in key order.
    /// A missing bound is unbounded.
    pub fn range<'a>(
        &'a self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> impl Iterator<Item = (&'a [u8], Option<&'a [u8]>)> {
        // BTreeMap::range panics on an empty or inverted range, so skip it entirely.
        let empty = matches!((start, end), (Some(s), Some(e)) if s >= e);
        let bounds = (
            start.map_or(Bound::Unbounded, Bound::Included),
            end.map_or(Bound::Unbounded, Bound::Excluded),
        );
        let iter = if empty {
            None
        } else {
            Some(self.entries.range::<[u8], _>(bounds))
        };
        iter.into_iter()
            .flatten()
            .map(|(k, v)| (k.as_slice(), v.as_deref()))
    }

    /// Bytes of keys and values held; will decide when to flush to an SSTable.
    #[allow(dead_code)]
    pub fn approximate_size(&self) -> usize {
        self.approximate_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_delete() {
        let mut m = MemTable::new();
        assert_eq!(m.get(b"a"), None);
        m.put(b"a".to_vec(), b"1".to_vec());
        assert_eq!(m.get(b"a"), Some(Some(&b"1"[..])));
        m.delete(b"a".to_vec());
        assert_eq!(m.get(b"a"), Some(None));
    }

    #[test]
    fn tracks_size() {
        let mut m = MemTable::new();
        m.put(b"ab".to_vec(), b"123".to_vec());
        assert_eq!(m.approximate_size(), 5);
        m.put(b"ab".to_vec(), b"1".to_vec());
        assert_eq!(m.approximate_size(), 3);
        m.delete(b"ab".to_vec());
        assert_eq!(m.approximate_size(), 2);
    }

    #[test]
    fn range_is_half_open_and_handles_inverted_bounds() {
        let mut m = MemTable::new();
        for k in [b"a", b"b", b"c", b"d"] {
            m.put(k.to_vec(), k.to_vec());
        }
        let keys = |s, e| m.range(s, e).map(|(k, _)| k.to_vec()).collect::<Vec<_>>();
        assert_eq!(
            keys(Some(b"b"), Some(b"d")),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(keys(None, Some(b"b")), vec![b"a".to_vec()]);
        assert_eq!(keys(Some(b"c"), None), vec![b"c".to_vec(), b"d".to_vec()]);
        assert!(keys(Some(b"c"), Some(b"c")).is_empty());
        assert!(keys(Some(b"d"), Some(b"a")).is_empty());
    }
}
