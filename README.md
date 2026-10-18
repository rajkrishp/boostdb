# boostdb

A key-value database built from scratch in Rust, growing toward an LSM-tree storage engine.

## Quick start

```sh
cargo test                  # run the test suite
cargo run -- ./my-data      # interactive shell on a data directory
```

```text
boostdb> put user:1 Ada Lovelace
OK
boostdb> get user:1
Ada Lovelace
boostdb> scan user: user;
user:1 = Ada Lovelace
(1 entries)
```

As a library:

```rust
let db = boostdb::Db::open("./my-data")?;
db.put(b"key", b"value")?;
assert_eq!(db.get(b"key")?, Some(b"value".to_vec()));
db.delete(b"key")?;
```

## How it works today

```text
 put/delete ──► WAL (wal.log, append + fsync) ──► MemTable (sorted, in memory)
 get/scan   ──────────────────────────────────────► MemTable
 open       ──► replay WAL into a fresh MemTable
```

| File | Role |
|---|---|
| `src/db.rs` | Public `Db` API, directory locking, recovery on open |
| `src/wal.rs` | Write-ahead log: record format, checksums, torn-tail truncation |
| `src/memtable.rs` | Sorted in-memory table with tombstones for deletes |
| `src/crc32.rs` | CRC-32 checksum for log records |
| `src/main.rs` | Interactive shell |

### WAL record format

All integers are little-endian. The CRC covers the payload.

```text
| crc32: u32 | payload_len: u32 | kind: u8 | key_len: u32 | value_len: u32 | key | value |
```

On open, records are replayed until the first incomplete or corrupt one. That point and
everything after it is truncated, because it can only come from a write cut off by a crash.

## Roadmap

1. **SSTables:** flush the memtable to immutable sorted files when it grows large, then truncate the WAL.
2. **Read path across levels:** check the memtable, then SSTables from newest to oldest.
3. **Compaction:** merge SSTables to drop overwritten values and tombstones.
4. **Bloom filters and block index:** skip SSTables that can't contain a key.
5. **Iterators and snapshots:** streaming scans and consistent point-in-time reads (MVCC sequence numbers).
6. **Batches and transactions:** atomic multi-key writes.
7. **Network server:** a simple TCP protocol so other processes can talk to boostdb.
