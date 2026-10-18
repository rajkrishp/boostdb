//! Merges sorted streams of entries into one, for scans that span the memtable and
//! several SSTables.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::Entry;
use crate::error::Result;

/// Yields each key once, in increasing order, with the value from the lowest-indexed
/// source that has it. Each source must be in strictly increasing key order. Pass sources
/// newest first, so newer values and tombstones shadow older ones.
pub struct MergeIter<I> {
    sources: Vec<I>,
    heap: BinaryHeap<Head>,
    started: bool,
    failed: bool,
}

/// The next unconsumed entry of one source.
struct Head {
    key: Vec<u8>,
    value: Option<Vec<u8>>,
    source: usize,
}

impl Ord for Head {
    // Reversed, so the max-heap pops the smallest key first and, among equal keys,
    // the newest source.
    fn cmp(&self, other: &Self) -> Ordering {
        (&other.key, other.source).cmp(&(&self.key, self.source))
    }
}

impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Head {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Head {}

impl<I: Iterator<Item = Result<Entry>>> MergeIter<I> {
    pub fn new(sources: Vec<I>) -> Self {
        MergeIter {
            heap: BinaryHeap::with_capacity(sources.len()),
            sources,
            started: false,
            failed: false,
        }
    }

    /// Pulls the next entry of `source` into the heap.
    fn advance(&mut self, source: usize) -> Result<()> {
        if let Some(entry) = self.sources[source].next() {
            let (key, value) = entry?;
            self.heap.push(Head { key, value, source });
        }
        Ok(())
    }

    fn step(&mut self) -> Result<Option<Entry>> {
        if !self.started {
            self.started = true;
            for source in 0..self.sources.len() {
                self.advance(source)?;
            }
        }
        let Some(newest) = self.heap.pop() else {
            return Ok(None);
        };
        self.advance(newest.source)?;
        // Older versions of the same key are shadowed: skip them.
        while self.heap.peek().is_some_and(|h| h.key == newest.key) {
            let older = self.heap.pop().unwrap();
            self.advance(older.source)?;
        }
        Ok(Some((newest.key, newest.value)))
    }
}

impl<I: Iterator<Item = Result<Entry>>> Iterator for MergeIter<I> {
    type Item = Result<Entry>;

    /// After an error, the iterator ends.
    fn next(&mut self) -> Option<Result<Entry>> {
        if self.failed {
            return None;
        }
        let result = self.step();
        self.failed = result.is_err();
        result.transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    fn source(entries: &[(&str, Option<&str>)]) -> std::vec::IntoIter<Result<Entry>> {
        entries
            .iter()
            .map(|(k, v)| Ok((k.as_bytes().to_vec(), v.map(|v| v.as_bytes().to_vec()))))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn newer_sources_shadow_older_ones() {
        let merged: Vec<Entry> = MergeIter::new(vec![
            source(&[("b", Some("new")), ("d", None)]),
            source(&[]),
            source(&[("a", Some("old")), ("b", Some("old")), ("d", Some("old"))]),
            source(&[("b", Some("oldest")), ("c", Some("oldest")), ("e", None)]),
        ])
        .collect::<Result<_>>()
        .unwrap();

        let expected: Vec<Entry> = [
            ("a", Some("old")),
            ("b", Some("new")),
            ("c", Some("oldest")),
            ("d", None),
            ("e", None),
        ]
        .iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.map(|v| v.as_bytes().to_vec())))
        .collect();
        assert_eq!(merged, expected);
    }

    #[test]
    fn stops_after_an_error() {
        let mut failing = source(&[("a", Some("1"))]).collect::<Vec<_>>();
        failing.push(Err(Error::Corruption("test".into())));
        let mut merged = MergeIter::new(vec![failing.into_iter(), source(&[("z", Some("2"))])]);

        assert!(matches!(merged.next(), Some(Err(Error::Corruption(_)))));
        assert!(merged.next().is_none());
    }
}
