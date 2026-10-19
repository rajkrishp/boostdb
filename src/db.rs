//! The public `Db` handle: write path, read path, flushing and recovery.
//!
//! A data directory holds numbered files: `NNNNNN.log` (write-ahead logs) and
//! `NNNNNN.sst` (SSTables). Every memtable is backed by one log, and flushing it writes an
//! SSTable with the same number as that log. So on open, any log numbered at or below the
//! newest SSTable has already been flushed and is deleted rather than replayed, and the
//! SSTables' numbers give their age.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::Entry;
use crate::error::{Error, Result};
use crate::memtable::MemTable;
use crate::merge::MergeIter;
use crate::sstable::{self, SsTable};
use crate::wal::{Record, Wal};

pub const MAX_KEY_SIZE: usize = 64 * 1024;
pub const MAX_VALUE_SIZE: usize = 64 * 1024 * 1024;

const LOCK_FILE: &str = "LOCK";
/// The log's name before logs were numbered; renamed to `000001.log` on open.
const LEGACY_WAL_FILE: &str = "wal.log";

#[derive(Debug, Clone)]
pub struct Options {
    /// fsync the log after every write. Slower, but a write that returned `Ok`
    /// survives a power loss. With `false`, it survives only a process crash.
    /// (Flushes to SSTables are always fsynced.)
    pub sync_writes: bool,
    /// Flush the memtable to an SSTable once its keys and values exceed this many bytes.
    /// The flush happens at the start of the next write, so an `Err` from a write always
    /// means that write was not applied.
    pub memtable_flush_threshold: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            sync_writes: true,
            memtable_flush_threshold: 4 * 1024 * 1024,
        }
    }
}

/// Counts of the work a [`Db`] has done since it was opened. They measure read and write
/// amplification in units that, unlike timings, come out the same on every machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// SSTables that `get` looked in, newest first, until one held the key.
    pub sstables_checked: u64,
    /// SSTable data blocks read from disk by `get` and `scan`.
    pub blocks_read: u64,
    /// Bytes appended to write-ahead logs.
    pub wal_bytes_written: u64,
    /// Bytes of SSTables written by memtable flushes.
    pub sstable_bytes_written: u64,
    /// Memtable flushes.
    pub flushes: u64,
}

/// A key-value database stored in a directory. Safe to share across threads.
pub struct Db {
    path: PathBuf,
    options: Options,
    inner: Mutex<Inner>,
    // Holds an exclusive OS lock on the directory for as long as the Db is open.
    _lock: File,
}

struct Inner {
    wal: Wal,
    /// The number of `wal`'s file, and of the SSTable its memtable will be flushed to.
    wal_number: u64,
    memtable: MemTable,
    /// Newest first.
    sstables: Vec<SsTable>,
    /// Every counter but `blocks_read`, which each SSTable keeps for itself.
    stats: Stats,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        Self::open_with(path, Options::default())
    }

    pub fn open_with(path: impl AsRef<Path>, options: Options) -> Result<Db> {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(&path)?;

        let lock = File::create(path.join(LOCK_FILE))?;
        lock.try_lock().map_err(|e| match e {
            fs::TryLockError::WouldBlock => Error::Locked,
            fs::TryLockError::Error(e) => Error::Io(e),
        })?;

        let legacy_wal = path.join(LEGACY_WAL_FILE);
        if legacy_wal.exists() {
            fs::rename(&legacy_wal, path.join(file_name(1, Kind::Log)))?;
        }

        let mut logs = Vec::new();
        let mut tables = Vec::new();
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            match entry.file_name().to_str().and_then(parse_file_name) {
                Some((n, Kind::Log)) => logs.push(n),
                Some((n, Kind::Table)) => tables.push(n),
                // Left by a crash partway through a flush; the log still has its data.
                Some((_, Kind::TempTable)) => fs::remove_file(entry.path())?,
                None => {}
            }
        }
        logs.sort_unstable();
        tables.sort_unstable_by(|a, b| b.cmp(a));
        let flushed = tables.first().copied().unwrap_or(0);
        // Open the SSTables before deleting any log they make obsolete.
        let sstables = tables
            .iter()
            .map(|&n| SsTable::open(&path.join(file_name(n, Kind::Table))))
            .collect::<Result<_>>()?;

        // Normally at most one log is newer than every SSTable: a flush creates the next
        // log only after its SSTable is durable. Replay any others in order, just in case.
        let mut memtable = MemTable::new();
        let mut current = None;
        for n in logs {
            let log_path = path.join(file_name(n, Kind::Log));
            if n <= flushed {
                // Left by a crash between writing the SSTable and deleting the log.
                fs::remove_file(log_path)?;
                continue;
            }
            let (wal, records) = Wal::open(&log_path, options.sync_writes)?;
            for record in records {
                match record {
                    Record::Put { key, value } => memtable.put(key, value),
                    Record::Delete { key } => memtable.delete(key),
                }
            }
            current = Some((wal, n));
        }
        let (wal, wal_number) = match current {
            Some(current) => current,
            None => {
                let n = flushed + 1;
                let (wal, _) = Wal::open(&path.join(file_name(n, Kind::Log)), options.sync_writes)?;
                (wal, n)
            }
        };
        sync_dir(&path)?;

        Ok(Db {
            path,
            options,
            inner: Mutex::new(Inner {
                wal,
                wal_number,
                memtable,
                sstables,
                stats: Stats::default(),
            }),
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        check_key(key)?;
        if value.len() > MAX_VALUE_SIZE {
            return Err(Error::ValueTooLarge(value.len()));
        }
        let mut inner = self.lock();
        self.make_room(&mut inner)?;
        let written = inner.wal.append(&Record::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        })?;
        inner.stats.wal_bytes_written += written as u64;
        inner.memtable.put(key.to_vec(), value.to_vec());
        Ok(())
    }

    pub fn delete(&self, key: &[u8]) -> Result<()> {
        check_key(key)?;
        let mut inner = self.lock();
        self.make_room(&mut inner)?;
        let written = inner.wal.append(&Record::Delete { key: key.to_vec() })?;
        inner.stats.wal_bytes_written += written as u64;
        inner.memtable.delete(key.to_vec());
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut guard = self.lock();
        if let Some(value) = guard.memtable.get(key) {
            return Ok(value.map(<[u8]>::to_vec));
        }
        let inner = &mut *guard;
        for table in &inner.sstables {
            inner.stats.sstables_checked += 1;
            if let Some(value) = table.get(key)? {
                return Ok(value);
            }
        }
        Ok(None)
    }

    /// Live key-value pairs with keys in `[start, end)`, in key order.
    /// A `None` bound is unbounded.
    pub fn scan(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let inner = self.lock();
        let mut sources: Vec<Box<dyn Iterator<Item = Result<Entry>> + '_>> =
            Vec::with_capacity(1 + inner.sstables.len());
        sources.push(Box::new(
            inner
                .memtable
                .range(start, end)
                .map(|(k, v)| Ok((k.to_vec(), v.map(<[u8]>::to_vec)))),
        ));
        for table in &inner.sstables {
            sources.push(Box::new(table.range(start, end)));
        }
        MergeIter::new(sources)
            .filter_map(|entry| match entry {
                Ok((key, Some(value))) => Some(Ok((key, value))),
                Ok((_, None)) => None,
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    /// Counters of the work done since the database was opened.
    pub fn stats(&self) -> Stats {
        let inner = self.lock();
        Stats {
            blocks_read: inner.sstables.iter().map(SsTable::blocks_read).sum(),
            ..inner.stats
        }
    }

    /// Flushes the memtable if it has outgrown the threshold.
    fn make_room(&self, inner: &mut Inner) -> Result<()> {
        if inner.memtable.approximate_size() <= self.options.memtable_flush_threshold {
            return Ok(());
        }
        // A failed flush may have left an SSTable on disk that makes the log obsolete.
        if inner.wal.is_poisoned() {
            return Err(Error::WalPoisoned);
        }
        self.flush(inner)
    }

    /// Writes the memtable to an SSTable, then switches to a fresh log and memtable.
    fn flush(&self, inner: &mut Inner) -> Result<()> {
        let number = inner.wal_number;
        let table_path = self.path.join(file_name(number, Kind::Table));
        let temp_path = self.path.join(file_name(number, Kind::TempTable));
        let written = match sstable::write(&temp_path, inner.memtable.range(None, None)) {
            Ok(written) => written,
            Err(e) => {
                let _ = fs::remove_file(&temp_path);
                return Err(e);
            }
        };

        // Once the rename may have happened, a reopen would treat the current log as
        // flushed, so writes appended to it from now on could be lost. If installing fails,
        // stop accepting writes; reopening recovers from whichever file survived.
        let (table, wal) = self
            .install(number, &temp_path, &table_path)
            .inspect_err(|_| inner.wal.poison())?;

        inner.sstables.insert(0, table);
        inner.wal = wal;
        inner.wal_number = number + 1;
        inner.memtable = MemTable::new();
        inner.stats.sstable_bytes_written += written;
        inner.stats.flushes += 1;
        // If this fails, the next open deletes it instead.
        let _ = fs::remove_file(self.path.join(file_name(number, Kind::Log)));
        Ok(())
    }

    /// Moves a finished SSTable into place and creates the log that follows it.
    fn install(&self, number: u64, temp_path: &Path, table_path: &Path) -> Result<(SsTable, Wal)> {
        fs::rename(temp_path, table_path)?;
        sync_dir(&self.path)?;
        let table = SsTable::open(table_path)?;
        let (wal, _) = Wal::open(
            &self.path.join(file_name(number + 1, Kind::Log)),
            self.options.sync_writes,
        )?;
        sync_dir(&self.path)?;
        Ok((table, wal))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock can't leave Inner half-updated in a way that
        // matters: the WAL poisons itself on failed writes, the memtable is updated last,
        // and a flush swaps in its new state only after everything on disk is in place.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn check_key(key: &[u8]) -> Result<()> {
    if key.len() > MAX_KEY_SIZE {
        return Err(Error::KeyTooLarge(key.len()));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Log,
    Table,
    /// An SSTable still being written.
    TempTable,
}

impl Kind {
    fn extension(self) -> &'static str {
        match self {
            Kind::Log => "log",
            Kind::Table => "sst",
            Kind::TempTable => "sst.tmp",
        }
    }
}

fn file_name(number: u64, kind: Kind) -> String {
    format!("{number:06}.{}", kind.extension())
}

fn parse_file_name(name: &str) -> Option<(u64, Kind)> {
    let (number, extension) = name.split_once('.')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let kind = [Kind::Log, Kind::Table, Kind::TempTable]
        .into_iter()
        .find(|k| k.extension() == extension)?;
    Some((number.parse().ok()?, kind))
}

/// Makes renames, creations and deletions of files in `dir` durable.
fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    // Windows can't open a directory as a file, and NTFS journals metadata anyway.
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_roundtrip() {
        for kind in [Kind::Log, Kind::Table, Kind::TempTable] {
            for n in [1, 42, 999_999, 1_000_000] {
                assert_eq!(parse_file_name(&file_name(n, kind)), Some((n, kind)));
            }
        }
        for other in ["LOCK", "wal.log", ".log", "12a.sst", "000001.txt", "000001"] {
            assert_eq!(parse_file_name(other), None, "{other}");
        }
    }
}
