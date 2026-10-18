use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::error::{Error, Result};
use crate::memtable::MemTable;
use crate::wal::{Record, Wal};

pub const MAX_KEY_SIZE: usize = 64 * 1024;
pub const MAX_VALUE_SIZE: usize = 64 * 1024 * 1024;

const WAL_FILE: &str = "wal.log";
const LOCK_FILE: &str = "LOCK";

#[derive(Debug, Clone)]
pub struct Options {
    /// fsync the log after every write. Slower, but a write that returned `Ok`
    /// survives a power loss. With `false`, it survives only a process crash.
    pub sync_writes: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { sync_writes: true }
    }
}

/// A key-value database stored in a directory. Safe to share across threads.
pub struct Db {
    path: PathBuf,
    inner: Mutex<Inner>,
    // Holds an exclusive OS lock on the directory for as long as the Db is open.
    _lock: File,
}

struct Inner {
    wal: Wal,
    memtable: MemTable,
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

        let (wal, records) = Wal::open(&path.join(WAL_FILE), options.sync_writes)?;
        let mut memtable = MemTable::new();
        for record in records {
            match record {
                Record::Put { key, value } => memtable.put(key, value),
                Record::Delete { key } => memtable.delete(key),
            }
        }

        Ok(Db {
            path,
            inner: Mutex::new(Inner { wal, memtable }),
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
        inner.wal.append(&Record::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        })?;
        inner.memtable.put(key.to_vec(), value.to_vec());
        Ok(())
    }

    pub fn delete(&self, key: &[u8]) -> Result<()> {
        check_key(key)?;
        let mut inner = self.lock();
        inner.wal.append(&Record::Delete { key: key.to_vec() })?;
        inner.memtable.delete(key.to_vec());
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.lock().memtable.get(key).flatten().map(<[u8]>::to_vec))
    }

    /// Live key-value pairs with keys in `[start, end)`, in key order.
    /// A `None` bound is unbounded.
    pub fn scan(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let inner = self.lock();
        Ok(inner
            .memtable
            .range(start, end)
            .filter_map(|(k, v)| Some((k.to_vec(), v?.to_vec())))
            .collect())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock can't leave Inner half-updated in a way that
        // matters: the WAL poisons itself on failed writes, and the memtable is updated last.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn check_key(key: &[u8]) -> Result<()> {
    if key.len() > MAX_KEY_SIZE {
        return Err(Error::KeyTooLarge(key.len()));
    }
    Ok(())
}
