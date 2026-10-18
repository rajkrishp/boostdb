//! boostdb: a key-value database built from scratch.
//!
//! Writes go to a write-ahead log (for durability) and then into an in-memory
//! sorted table (for fast reads). See `README.md` for the architecture and roadmap.

mod crc32;
mod db;
mod error;
mod memtable;
mod wal;

pub use db::{Db, MAX_KEY_SIZE, MAX_VALUE_SIZE, Options};
pub use error::{Error, Result};
