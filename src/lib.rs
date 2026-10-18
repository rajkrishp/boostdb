//! boostdb: a key-value database built from scratch.
//!
//! Writes go to a write-ahead log (for durability) and then into an in-memory sorted
//! table (for fast reads). When that table grows large it is flushed to an immutable
//! sorted file on disk (an SSTable), and reads consult the memtable and then the SSTables
//! from newest to oldest. See `README.md` for the architecture and roadmap.

mod crc32;
mod db;
mod error;
mod memtable;
mod merge;
mod sstable;
mod wal;

pub use db::{Db, MAX_KEY_SIZE, MAX_VALUE_SIZE, Options};
pub use error::{Error, Result};

/// A key and its value, or `None` for a tombstone.
type Entry = (Vec<u8>, Option<Vec<u8>>);
