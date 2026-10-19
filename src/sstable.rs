//! Sorted string tables: immutable, sorted on-disk files that hold flushed memtables.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! | data block 0 | data block 1 | ... | index block | footer |
//!
//! data block  = | entry ... | crc32: u32 |
//! entry       = | kind: u8 | key_len: u32 | value_len: u32 | key ... | value ... |
//! index block = | index entry ... | crc32: u32 |
//! index entry = | last_key_len: u32 | last_key ... | block_offset: u64 | block_len: u32 |
//! footer      = | index_offset: u64 | index_len: u64 | magic: [u8; 8] |
//! ```
//!
//! Entries are in strictly increasing key order. `kind` is 1 for a value and 2 for a
//! tombstone (whose `value_len` is 0). Each block's CRC covers the bytes before it, and
//! `block_len` and `index_len` include the CRC. The index records every block's last key,
//! so a lookup binary-searches it to find the one block that can hold a key.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Entry;
use crate::crc32::crc32;
use crate::error::{Error, Result};

/// A data block is closed once its entries reach this many bytes.
const BLOCK_SIZE: usize = 4096;
const FOOTER_LEN: usize = 24;
const CRC_LEN: usize = 4;
const MAGIC: [u8; 8] = *b"BSTABLE1";
const KIND_VALUE: u8 = 1;
const KIND_TOMBSTONE: u8 = 2;

/// Writes `entries`, which must be in strictly increasing key order, to a new SSTable at
/// `path` and fsyncs it, returning its size in bytes. A `None` value is a tombstone.
pub fn write<'a>(
    path: &Path,
    entries: impl IntoIterator<Item = (&'a [u8], Option<&'a [u8]>)>,
) -> Result<u64> {
    let mut writer = Writer {
        out: BufWriter::new(File::create(path)?),
        offset: 0,
        block: Vec::with_capacity(2 * BLOCK_SIZE),
        last_key: Vec::new(),
        index: Vec::new(),
    };
    for (key, value) in entries {
        writer.add(key, value)?;
    }
    let (file, len) = writer.finish()?;
    file.sync_all()?;
    Ok(len)
}

struct Writer {
    out: BufWriter<File>,
    /// Bytes written to `out` so far, i.e. where the next block starts.
    offset: u64,
    block: Vec<u8>,
    last_key: Vec<u8>,
    index: Vec<u8>,
}

impl Writer {
    fn add(&mut self, key: &[u8], value: Option<&[u8]>) -> io::Result<()> {
        debug_assert!(
            (self.offset == 0 && self.block.is_empty()) || key > self.last_key.as_slice(),
            "SSTable keys must be strictly increasing"
        );
        let (kind, value) = match value {
            Some(v) => (KIND_VALUE, v),
            None => (KIND_TOMBSTONE, &[][..]),
        };
        self.block.push(kind);
        self.block
            .extend_from_slice(&(key.len() as u32).to_le_bytes());
        self.block
            .extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.block.extend_from_slice(key);
        self.block.extend_from_slice(value);
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        if self.block.len() >= BLOCK_SIZE {
            self.finish_block()?;
        }
        Ok(())
    }

    fn finish_block(&mut self) -> io::Result<()> {
        let crc = crc32(&self.block);
        self.block.extend_from_slice(&crc.to_le_bytes());
        self.out.write_all(&self.block)?;

        self.index
            .extend_from_slice(&(self.last_key.len() as u32).to_le_bytes());
        self.index.extend_from_slice(&self.last_key);
        self.index.extend_from_slice(&self.offset.to_le_bytes());
        self.index
            .extend_from_slice(&(self.block.len() as u32).to_le_bytes());

        self.offset += self.block.len() as u64;
        self.block.clear();
        Ok(())
    }

    /// Writes the index and footer, returning the file and its total length.
    fn finish(mut self) -> io::Result<(File, u64)> {
        if !self.block.is_empty() {
            self.finish_block()?;
        }
        let crc = crc32(&self.index);
        self.index.extend_from_slice(&crc.to_le_bytes());
        self.out.write_all(&self.index)?;
        self.out.write_all(&self.offset.to_le_bytes())?;
        self.out
            .write_all(&(self.index.len() as u64).to_le_bytes())?;
        self.out.write_all(&MAGIC)?;
        let len = self.offset + self.index.len() as u64 + FOOTER_LEN as u64;
        let file = self
            .out
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        Ok((file, len))
    }
}

/// An open SSTable. Its index is held in memory; data blocks are read on demand.
pub struct SsTable {
    path: PathBuf,
    file: File,
    index: Vec<BlockHandle>,
    /// Data blocks read since the table was opened, for `Db::stats`.
    blocks_read: AtomicU64,
}

struct BlockHandle {
    last_key: Vec<u8>,
    offset: u64,
    len: u32,
}

impl SsTable {
    pub fn open(path: &Path) -> Result<SsTable> {
        let corrupt = |what: &str| Error::Corruption(format!("{}: {what}", path.display()));
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let Some(index_end) = file_len.checked_sub(FOOTER_LEN as u64) else {
            return Err(corrupt("too short to be an SSTable"));
        };

        let mut footer = [0; FOOTER_LEN];
        read_exact_at(&file, &mut footer, index_end)?;
        if footer[16..] != MAGIC {
            return Err(corrupt("bad magic number"));
        }
        let index_offset = u64::from_le_bytes(footer[0..8].try_into().unwrap());
        let index_len = u64::from_le_bytes(footer[8..16].try_into().unwrap());
        if index_offset.checked_add(index_len) != Some(index_end) {
            return Err(corrupt("footer does not match the file length"));
        }

        let mut index = vec![0; index_len as usize];
        read_exact_at(&file, &mut index, index_offset)?;
        let index = verify_crc(&index)
            .and_then(|data| decode_index(data, index_offset))
            .ok_or_else(|| corrupt("index block is corrupt"))?;

        Ok(SsTable {
            path: path.to_path_buf(),
            file,
            index,
            blocks_read: AtomicU64::new(0),
        })
    }

    /// Data blocks read from disk since the table was opened.
    pub fn blocks_read(&self) -> u64 {
        self.blocks_read.load(Ordering::Relaxed)
    }

    /// `None` if the key is not in this table, `Some(None)` if it holds a tombstone,
    /// `Some(Some(value))` if it holds a value.
    pub fn get(&self, key: &[u8]) -> Result<Option<Option<Vec<u8>>>> {
        let i = self.block_for(key);
        if i == self.index.len() {
            return Ok(None);
        }
        let mut entries = self.read_block(i)?;
        Ok(entries
            .binary_search_by(|(k, _)| k.as_slice().cmp(key))
            .ok()
            .map(|i| entries.swap_remove(i).1))
    }

    /// Entries (including tombstones) with keys in `[start, end)`, in key order.
    /// A missing bound is unbounded. Blocks are read lazily as the iterator advances.
    pub fn range(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Iter<'_> {
        Iter {
            table: self,
            next_block: start.map_or(0, |s| self.block_for(s)),
            entries: Vec::new().into_iter(),
            start: start.map(<[u8]>::to_vec),
            end: end.map(<[u8]>::to_vec),
            done: false,
        }
    }

    /// The first block whose last key is `>= key`: the only one that can hold `key`.
    fn block_for(&self, key: &[u8]) -> usize {
        self.index.partition_point(|h| h.last_key.as_slice() < key)
    }

    fn read_block(&self, i: usize) -> Result<Vec<Entry>> {
        let handle = &self.index[i];
        let mut buf = vec![0; handle.len as usize];
        self.blocks_read.fetch_add(1, Ordering::Relaxed);
        read_exact_at(&self.file, &mut buf, handle.offset)?;
        verify_crc(&buf)
            .and_then(decode_block)
            // The index's last key doubles as a check that this is the block it describes.
            .filter(|entries| entries.last().is_some_and(|(k, _)| *k == handle.last_key))
            .ok_or_else(|| {
                Error::Corruption(format!(
                    "{}: data block at offset {} is corrupt",
                    self.path.display(),
                    handle.offset
                ))
            })
    }
}

/// Iterator returned by [`SsTable::range`].
pub struct Iter<'a> {
    table: &'a SsTable,
    next_block: usize,
    entries: std::vec::IntoIter<Entry>,
    /// Cleared once passed: only the first block read can hold keys below it.
    start: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
    done: bool,
}

impl Iterator for Iter<'_> {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Result<Entry>> {
        while !self.done {
            if let Some((key, value)) = self.entries.next() {
                if let Some(start) = &self.start {
                    if key < *start {
                        continue;
                    }
                    self.start = None;
                }
                if self.end.as_ref().is_some_and(|end| key >= *end) {
                    break;
                }
                return Some(Ok((key, value)));
            }
            if self.next_block == self.table.index.len() {
                break;
            }
            match self.table.read_block(self.next_block) {
                Ok(entries) => {
                    self.entries = entries.into_iter();
                    self.next_block += 1;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
        self.done = true;
        None
    }
}

/// Returns the bytes before a trailing CRC-32 if they match it.
fn verify_crc(buf: &[u8]) -> Option<&[u8]> {
    let (data, crc) = buf.split_at_checked(buf.len().checked_sub(CRC_LEN)?)?;
    (crc32(data) == u32::from_le_bytes(crc.try_into().unwrap())).then_some(data)
}

/// Decodes the index, checking that blocks are non-empty, contiguous, end where the index
/// starts, and have strictly increasing last keys.
fn decode_index(data: &[u8], data_end: u64) -> Option<Vec<BlockHandle>> {
    let mut r = Cursor(data);
    let mut handles: Vec<BlockHandle> = Vec::new();
    let mut expected_offset = 0;
    while !r.0.is_empty() {
        let key_len = r.u32()? as usize;
        let last_key = r.bytes(key_len)?.to_vec();
        let offset = r.u64()?;
        let len = r.u32()?;
        let in_order = handles.last().is_none_or(|prev| prev.last_key < last_key);
        if offset != expected_offset || (len as usize) <= CRC_LEN || !in_order {
            return None;
        }
        expected_offset += len as u64;
        handles.push(BlockHandle {
            last_key,
            offset,
            len,
        });
    }
    (expected_offset == data_end).then_some(handles)
}

fn decode_block(data: &[u8]) -> Option<Vec<Entry>> {
    let mut r = Cursor(data);
    let mut entries: Vec<Entry> = Vec::new();
    while !r.0.is_empty() {
        let kind = r.u8()?;
        let key_len = r.u32()? as usize;
        let value_len = r.u32()? as usize;
        let key = r.bytes(key_len)?.to_vec();
        let value = r.bytes(value_len)?;
        let value = match kind {
            KIND_VALUE => Some(value.to_vec()),
            KIND_TOMBSTONE if value.is_empty() => None,
            _ => return None,
        };
        if entries.last().is_some_and(|(prev, _)| *prev >= key) {
            return None;
        }
        entries.push((key, value));
    }
    Some(entries)
}

/// Reads little-endian fields off the front of a byte slice.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
}

/// Positional read that doesn't move a shared file cursor, so `&SsTable` can be read from
/// several places at once.
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use super::*;

    fn build(path: &Path, entries: &[Entry]) -> SsTable {
        write(
            path,
            entries.iter().map(|(k, v)| (k.as_slice(), v.as_deref())),
        )
        .unwrap();
        SsTable::open(path).unwrap()
    }

    fn collect(iter: Iter<'_>) -> Vec<Entry> {
        iter.collect::<Result<_>>().unwrap()
    }

    /// 2000 keys with values of varying size; every seventh key is a tombstone.
    fn sample() -> Vec<Entry> {
        (0..2000u32)
            .map(|i| {
                let key = format!("key{i:05}").into_bytes();
                let value = (i % 7 != 0).then(|| vec![b'a' + (i % 26) as u8; (i % 50) as usize]);
                (key, value)
            })
            .collect()
    }

    #[test]
    fn point_lookups_across_many_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let entries = sample();
        let table = build(&dir.path().join("t.sst"), &entries);
        assert!(table.index.len() > 5, "expected several blocks");

        for (key, value) in &entries {
            assert_eq!(table.get(key).unwrap(), Some(value.clone()));
        }
        for missing in [&b""[..], b"a", b"key00010x", b"key99999", b"zzz"] {
            assert_eq!(table.get(missing).unwrap(), None);
        }
    }

    #[test]
    fn range_scans_honor_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let entries = sample();
        let table = build(&dir.path().join("t.sst"), &entries);

        assert_eq!(collect(table.range(None, None)), entries);
        type Bound<'a> = Option<&'a [u8]>;
        let bounds: [(Bound, Bound); 6] = [
            (Some(b"key00500"), Some(b"key01500")),
            (Some(b"key00500x"), Some(b"key00501x")),
            (None, Some(b"key00003")),
            (Some(b"key01998"), None),
            (Some(b"key01000"), Some(b"key01000")),
            (Some(b"zzz"), None),
        ];
        for (start, end) in bounds {
            let expected: Vec<Entry> = entries
                .iter()
                .filter(|(k, _)| start.is_none_or(|s| k.as_slice() >= s))
                .filter(|(k, _)| end.is_none_or(|e| k.as_slice() < e))
                .cloned()
                .collect();
            assert_eq!(
                collect(table.range(start, end)),
                expected,
                "{start:?}..{end:?}"
            );
        }
    }

    #[test]
    fn empty_table() {
        let dir = tempfile::tempdir().unwrap();
        let table = build(&dir.path().join("t.sst"), &[]);
        assert_eq!(table.get(b"a").unwrap(), None);
        assert!(collect(table.range(None, None)).is_empty());
    }

    #[test]
    fn value_larger_than_a_block() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            (b"a".to_vec(), Some(vec![1; 3 * BLOCK_SIZE])),
            (b"b".to_vec(), None),
            (b"c".to_vec(), Some(vec![2; 10])),
        ];
        let table = build(&dir.path().join("t.sst"), &entries);
        assert_eq!(table.get(b"a").unwrap(), Some(entries[0].1.clone()));
        assert_eq!(table.get(b"b").unwrap(), Some(None));
        assert_eq!(collect(table.range(None, None)), entries);
    }

    #[test]
    fn detects_corrupt_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sst");
        let entries = sample();
        build(&path, &entries);

        let mut data = std::fs::read(&path).unwrap();
        data[10] ^= 0xFF; // inside the first data block
        std::fs::write(&path, &data).unwrap();

        let table = SsTable::open(&path).unwrap();
        assert!(matches!(
            table.get(&entries[0].0),
            Err(Error::Corruption(_))
        ));
        assert!(matches!(
            table.range(None, None).next(),
            Some(Err(Error::Corruption(_)))
        ));
        // Blocks other than the damaged one are still readable.
        let last = &entries.last().unwrap();
        assert_eq!(table.get(&last.0).unwrap(), Some(last.1.clone()));
    }

    #[test]
    fn rejects_truncated_or_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sst");
        build(&path, &sample());
        let len = std::fs::metadata(&path).unwrap().len();

        for cut in [1, FOOTER_LEN as u64, len / 2, len - 1, len] {
            let truncated = dir.path().join("truncated.sst");
            std::fs::copy(&path, &truncated).unwrap();
            OpenOptions::new()
                .write(true)
                .open(&truncated)
                .unwrap()
                .set_len(len - cut)
                .unwrap();
            assert!(
                matches!(SsTable::open(&truncated), Err(Error::Corruption(_))),
                "cut {cut} bytes"
            );
        }

        let mut data = std::fs::read(&path).unwrap();
        let n = data.len();
        data[n - FOOTER_LEN - 1] ^= 0xFF; // inside the index's CRC
        std::fs::write(&path, &data).unwrap();
        assert!(matches!(SsTable::open(&path), Err(Error::Corruption(_))));
    }
}
