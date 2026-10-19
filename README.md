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
println!("{:?}", db.stats()); // counters: SSTables checked, blocks read, bytes written, ...
```

## How it works today

```text
 put/delete ──► make room ──► WAL (NNNNNN.log, append + fsync) ──► MemTable (sorted, in memory)
                    │
                    └─ memtable over threshold? ──► flush to NNNNNN.sst, start the next log

 get        ──► MemTable ──► newest .sst ──► … ──► oldest .sst     (first hit wins, tombstones included)
 scan       ──► k-way merge of MemTable + every .sst, newest source wins per key, tombstones dropped

 open       ──► open every .sst ──► delete logs already flushed ──► replay the remaining log
```

Writes go to the log and then the memtable. Before a write, if the memtable holds more than
`Options::memtable_flush_threshold` bytes (default 4 MiB) of keys and values, it is written out
as an immutable SSTable and a fresh log and memtable take over. Flushing before the write, not
after, means an `Err` from `put`/`delete` always means the write was not applied.

| File | Role |
|---|---|
| `src/db.rs` | Public `Db` API, directory locking, flushing, recovery on open |
| `src/wal.rs` | Write-ahead log: record format, checksums, torn-tail truncation |
| `src/memtable.rs` | Sorted in-memory table with tombstones for deletes |
| `src/sstable.rs` | SSTable writer and reader: data blocks, block index, footer |
| `src/merge.rs` | K-way merge of sorted sources for scans, newest source wins |
| `src/crc32.rs` | CRC-32 checksum for log records and SSTable blocks |
| `src/main.rs` | Interactive shell |
| `bench/` | Benchmark harness and `run_all.sh` (see [Benchmarks](#benchmarks)) |

## On-disk format

All integers are little-endian.

### Data directory

```text
LOCK          exclusive OS lock held while a Db is open
000007.log    write-ahead log behind the current memtable
000006.sst    SSTables, one per flushed memtable; a higher number is newer
000003.sst
```

Each memtable is backed by one log, and flushing it writes an SSTable with **the same number
as that log**. A flush of log *N*:

1. writes `N.sst.tmp`, fsyncs it, renames it to `N.sst`, fsyncs the directory;
2. creates `N+1.log`, fsyncs the directory, switches writes to it;
3. deletes `N.log`.

On open, any log numbered at or below the newest SSTable has already been flushed, so it is
deleted rather than replayed, and leftover `.sst.tmp` files are deleted. A crash at any point
in a flush therefore neither loses writes (the log is deleted only after the SSTable is
durable) nor replays them twice (a flushed log is recognized by its number). If a flush fails
after the rename may have happened, the database refuses further writes until it is reopened,
since appending to a log that a reopen would treat as flushed could lose those writes.

A data directory from before logs were numbered (`wal.log`) is picked up as `000001.log`.

### WAL record format

The CRC covers the payload.

```text
| crc32: u32 | payload_len: u32 | kind: u8 | key_len: u32 | value_len: u32 | key | value |
```

On open, records are replayed until the first incomplete or corrupt one. That point and
everything after it is truncated, because it can only come from a write cut off by a crash.

### SSTable format

```text
| data block 0 | data block 1 | ... | index block | footer |

data block  = | entry ... | crc32: u32 |
entry       = | kind: u8 | key_len: u32 | value_len: u32 | key | value |
index block = | index entry ... | crc32: u32 |
index entry = | last_key_len: u32 | last_key | block_offset: u64 | block_len: u32 |
footer      = | index_offset: u64 | index_len: u64 | magic: "BSTABLE1" |
```

Entries are in strictly increasing key order; `kind` is 1 for a value and 2 for a tombstone.
A block is closed once its entries reach 4 KiB. Each CRC covers the bytes of its block before
it, and `block_len`/`index_len` include the CRC. Opening a table checks the footer and index
and keeps the index in memory; a point lookup binary-searches it for the first block whose
last key is `>=` the key, reads that one block, and verifies its checksum. Range scans read
blocks lazily, one at a time. A checksum mismatch surfaces as `Error::Corruption`.

## Benchmarks

`bench/` is a separate crate that produces the same scorecard for every tutorial part. Each
part is a git tag (`part-01-wal`, `part-02-sstables`, ...), and the harness uses only the
public API every part has, so it builds and runs unchanged against all of them.

```sh
bench/run_all.sh                         # every part-* tag, 1M keys (1-3 minutes per part)
bench/run_all.sh -- --keys 50000 --reps 3   # a quick run; options go after --
bench/run_all.sh part-02-sstables HEAD   # only these refs (committed code only)
```

For each ref, `run_all.sh` checks it out into a temporary git worktree, copies the current
harness into it, builds it in release mode, and writes `bench/results/<ref>.json`. Then it
writes `bench/results/summary.md`, a table comparing every JSON file in that directory, with
each part's change from the previous one. To run the harness on the working tree directly:

```sh
cd bench && cargo run --release -- --keys 100000 --out /tmp/now.json
cargo run --release -- summary results    # the comparison table, from any directory of JSON
cargo run --release -- --help             # every option
```

### What it measures

Workloads use 16-byte keys and 100-byte values chosen by a seeded PRNG, so every run does the
same work. Each timed workload runs in a child process (so its peak memory is its own), five
times by default, and the median is reported.

| Workload | Metrics |
|---|---|
| Load 1M keys in random key order, `sync_writes` off | throughput, peak memory, write amplification, disk size vs. live data (space amplification), file count |
| 500 writes with `sync_writes` on | throughput |
| Open the loaded database | recovery time, peak memory |
| Point reads, half hits and half misses | p50/p99 latency of each, SSTables checked and blocks read per get |
| Range scans of about 100 keys | p50/p99 latency, blocks read per scan |
| Overwrite: 1M writes to random existing keys | throughput, write and space amplification, then read latency and SSTables per get |
| Delete half the keys | throughput, space amplification |
| Crash check: SIGKILL a writer (sync on) at random moments, 20 times | acknowledged writes lost, failed reopens |

Prefer the counted metrics (amplification, SSTables and blocks per get, files, lost writes):
they come out the same on any machine. Timings depend on the machine; each JSON file records
the CPU, memory, OS and rustc it ran with.

The counters come from `Db::stats()`, which parts before it don't have: `run_all.sh` builds
the harness without its `stats` feature for those, and their counters are `null`. The crash
check kills the process, not the machine, so it tests recovery logic rather than whether
fsync reaches the disk. Its values are 16 KiB so that writes cross the 4 MiB flush threshold
and kills land during flushes too.

The JSON schema is versioned (`schema_version`). Adding a metric keeps the version; renaming
or changing one bumps it.

## Roadmap

1. ~~**SSTables:** flush the memtable to immutable sorted files when it grows large, then truncate the WAL.~~ Done.
2. ~~**Read path across levels:** check the memtable, then SSTables from newest to oldest.~~ Done.
3. **Compaction:** merge SSTables to drop overwritten values and tombstones. Needs a MANIFEST: once one merged table replaces several, file numbers alone no longer give each table's age, and the swap must be atomic.
4. **Bloom filters:** skip SSTables that can't contain a key (the block index already exists).
5. **Iterators and snapshots:** streaming scans and consistent point-in-time reads (MVCC sequence numbers).
6. **Batches and transactions:** atomic multi-key writes.
7. **Network server:** a simple TCP protocol so other processes can talk to boostdb.
