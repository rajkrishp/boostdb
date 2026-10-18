//! Write-ahead log: every mutation is appended here before it is applied in memory,
//! so the in-memory state can be rebuilt after a crash.
//!
//! Record layout (all integers little-endian):
//!
//! ```text
//! | crc32: u32 | payload_len: u32 | payload ...                                 |
//! payload = | kind: u8 | key_len: u32 | value_len: u32 | key ... | value ... |
//! ```
//!
//! The CRC covers the payload. On open, records are read until the first one that is
//! incomplete or fails its checksum; that point and everything after it is truncated,
//! since it can only come from a write interrupted by a crash.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use crate::crc32::crc32;
use crate::error::{Error, Result};

const HEADER_LEN: usize = 8;
const PAYLOAD_FIXED_LEN: usize = 9;
const KIND_PUT: u8 = 1;
const KIND_DELETE: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

impl Record {
    fn encode(&self) -> Vec<u8> {
        let (kind, key, value): (u8, &[u8], &[u8]) = match self {
            Record::Put { key, value } => (KIND_PUT, key, value),
            Record::Delete { key } => (KIND_DELETE, key, &[]),
        };
        let payload_len = PAYLOAD_FIXED_LEN + key.len() + value.len();
        let mut buf = Vec::with_capacity(HEADER_LEN + payload_len);
        buf.extend_from_slice(&[0; 4]); // crc, filled in below
        buf.extend_from_slice(&(payload_len as u32).to_le_bytes());
        buf.push(kind);
        buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        buf.extend_from_slice(key);
        buf.extend_from_slice(value);
        let crc = crc32(&buf[HEADER_LEN..]);
        buf[..4].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    fn decode(payload: &[u8]) -> Option<Record> {
        let fixed = payload.get(..PAYLOAD_FIXED_LEN)?;
        let kind = fixed[0];
        let key_len = u32::from_le_bytes(fixed[1..5].try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(fixed[5..9].try_into().unwrap()) as usize;
        if payload.len() != PAYLOAD_FIXED_LEN + key_len + value_len {
            return None;
        }
        let key = payload[PAYLOAD_FIXED_LEN..PAYLOAD_FIXED_LEN + key_len].to_vec();
        let value = &payload[PAYLOAD_FIXED_LEN + key_len..];
        match kind {
            KIND_PUT => Some(Record::Put {
                key,
                value: value.to_vec(),
            }),
            KIND_DELETE if value.is_empty() => Some(Record::Delete { key }),
            _ => None,
        }
    }
}

/// Reads one record from the front of `buf`, returning it and its encoded length.
fn read_record(buf: &[u8]) -> Option<(Record, usize)> {
    let header = buf.get(..HEADER_LEN)?;
    let crc = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let payload_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let payload = buf.get(HEADER_LEN..HEADER_LEN + payload_len)?;
    if crc32(payload) != crc {
        return None;
    }
    Some((Record::decode(payload)?, HEADER_LEN + payload_len))
}

pub struct Wal {
    file: File,
    sync: bool,
    poisoned: bool,
}

impl Wal {
    /// Opens (or creates) the log at `path`, returning it along with every intact record.
    pub fn open(path: &Path, sync: bool) -> Result<(Wal, Vec<Record>)> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;

        let mut records = Vec::new();
        let mut offset = 0;
        while let Some((record, len)) = read_record(&data[offset..]) {
            records.push(record);
            offset += len;
        }
        if offset < data.len() {
            file.set_len(offset as u64)?;
            file.sync_all()?;
        }

        Ok((
            Wal {
                file,
                sync,
                poisoned: false,
            },
            records,
        ))
    }

    pub fn append(&mut self, record: &Record) -> Result<()> {
        if self.poisoned {
            return Err(Error::WalPoisoned);
        }
        let result = self.file.write_all(&record.encode()).and_then(|()| {
            if self.sync {
                self.file.sync_data()
            } else {
                Ok(())
            }
        });
        // After a failed write or fsync the file may hold a partial record, and anything
        // appended after it would be lost on replay. Stop accepting writes instead.
        if result.is_err() {
            self.poisoned = true;
        }
        Ok(result?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        for record in [
            Record::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            },
            Record::Put {
                key: vec![],
                value: vec![],
            },
            Record::Delete {
                key: b"gone".to_vec(),
            },
        ] {
            let buf = record.encode();
            assert_eq!(read_record(&buf), Some((record, buf.len())));
        }
    }

    #[test]
    fn rejects_truncated_and_corrupt_records() {
        let buf = Record::Put {
            key: b"key".to_vec(),
            value: b"value".to_vec(),
        }
        .encode();
        for cut in 0..buf.len() {
            assert_eq!(read_record(&buf[..cut]), None, "truncated at {cut}");
        }
        let mut corrupt = buf.clone();
        *corrupt.last_mut().unwrap() ^= 0xFF;
        assert_eq!(read_record(&corrupt), None);
    }
}
