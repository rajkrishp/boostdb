use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use boostdb::{Db, Error, MAX_KEY_SIZE, Options};

fn kv(k: &str, v: &str) -> (Vec<u8>, Vec<u8>) {
    (k.as_bytes().to_vec(), v.as_bytes().to_vec())
}

fn with_threshold(memtable_flush_threshold: usize) -> Options {
    Options {
        memtable_flush_threshold,
        ..Options::default()
    }
}

/// With a threshold of 0, every write flushes whatever the memtable holds first, so each
/// write but the latest ends up in an SSTable of its own.
fn flush_every_write() -> Options {
    with_threshold(0)
}

/// Names of the files in `dir` with the given extension, sorted.
fn files(dir: &Path, extension: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(&format!(".{extension}")))
        .collect();
    names.sort();
    names
}

#[test]
fn put_get_overwrite_delete() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();

    assert_eq!(db.get(b"a").unwrap(), None);
    db.put(b"a", b"1").unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
    db.put(b"a", b"2").unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"2".to_vec()));
    db.delete(b"a").unwrap();
    assert_eq!(db.get(b"a").unwrap(), None);
    db.delete(b"never-existed").unwrap();
}

#[test]
fn scan_skips_deleted_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    for (k, v) in [("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")] {
        db.put(k.as_bytes(), v.as_bytes()).unwrap();
    }
    db.delete(b"c").unwrap();

    assert_eq!(
        db.scan(None, None).unwrap(),
        vec![kv("a", "1"), kv("b", "2"), kv("d", "4")]
    );
    assert_eq!(db.scan(Some(b"b"), Some(b"d")).unwrap(), vec![kv("b", "2")]);
}

#[test]
fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path()).unwrap();
        db.put(b"kept", b"yes").unwrap();
        db.put(b"removed", b"soon").unwrap();
        db.delete(b"removed").unwrap();
        db.put(b"kept", b"updated").unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(db.get(b"kept").unwrap(), Some(b"updated".to_vec()));
    assert_eq!(db.get(b"removed").unwrap(), None);
}

#[test]
fn recovers_from_torn_log_tail() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path()).unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
    }
    // Simulate a crash partway through writing a record.
    let wal = dir.path().join("000001.log");
    OpenOptions::new()
        .append(true)
        .open(&wal)
        .unwrap()
        .write_all(&[0xAB; 11])
        .unwrap();

    {
        let db = Db::open(dir.path()).unwrap();
        assert_eq!(
            db.scan(None, None).unwrap(),
            vec![kv("a", "1"), kv("b", "2")]
        );
        // Writes after recovery must land after the truncated tail, not behind the garbage.
        db.put(b"c", b"3").unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(
        db.scan(None, None).unwrap(),
        vec![kv("a", "1"), kv("b", "2"), kv("c", "3")]
    );
}

#[test]
fn drops_partially_written_last_record() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path()).unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
    }
    let wal = dir.path().join("000001.log");
    let len = std::fs::metadata(&wal).unwrap().len();
    OpenOptions::new()
        .write(true)
        .open(&wal)
        .unwrap()
        .set_len(len - 3)
        .unwrap();

    let db = Db::open(dir.path()).unwrap();
    assert_eq!(db.scan(None, None).unwrap(), vec![kv("a", "1")]);
}

#[test]
fn second_open_of_same_dir_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let _db = Db::open(dir.path()).unwrap();
    assert!(matches!(Db::open(dir.path()), Err(Error::Locked)));
}

#[test]
fn rejects_oversized_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    let big = vec![0u8; MAX_KEY_SIZE + 1];
    assert!(matches!(db.put(&big, b"v"), Err(Error::KeyTooLarge(_))));
}

#[test]
fn concurrent_writers() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    std::thread::scope(|s| {
        for t in 0..4 {
            let db = &db;
            s.spawn(move || {
                for i in 0..100 {
                    db.put(format!("t{t}-{i:03}").as_bytes(), b"x").unwrap();
                }
            });
        }
    });
    assert_eq!(db.scan(None, None).unwrap().len(), 400);
}

#[test]
fn flush_happens_at_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path(), with_threshold(1000)).unwrap();
    let value = [b'v'; 96]; // with a 4-byte key, each entry is 100 bytes

    for i in 0..40 {
        db.put(format!("k{i:03}").as_bytes(), &value).unwrap();
        // The memtable passes 1000 bytes on its 11th entry, and the 12th write flushes
        // those 11 before going into a fresh memtable.
        assert_eq!(files(dir.path(), "sst").len(), i / 11, "after write {i}");
    }
    assert_eq!(files(dir.path(), "log"), vec!["000004.log"]);
    for i in 0..40 {
        assert_eq!(
            db.get(format!("k{i:03}").as_bytes()).unwrap(),
            Some(value.to_vec())
        );
    }
}

#[test]
fn data_survives_reopen_after_flush() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_with(dir.path(), with_threshold(200)).unwrap();
        for i in 0..100 {
            db.put(
                format!("key{:02}", i % 30).as_bytes(),
                format!("v{i}").as_bytes(),
            )
            .unwrap();
        }
        db.delete(b"key05").unwrap();
    }
    assert!(files(dir.path(), "sst").len() > 1);

    let db = Db::open(dir.path()).unwrap();
    let entries = db.scan(None, None).unwrap();
    assert_eq!(entries.len(), 29);
    assert_eq!(db.get(b"key05").unwrap(), None);
    // The last write to key i % 30 was i = 70 + i % 30.
    assert_eq!(db.get(b"key00").unwrap(), Some(b"v90".to_vec()));
    assert_eq!(db.get(b"key29").unwrap(), Some(b"v89".to_vec()));
}

#[test]
fn deletes_shadow_older_sstable_values() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_with(dir.path(), flush_every_write()).unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"1").unwrap(); // a=1 is now in an SSTable
        db.delete(b"a").unwrap(); // the tombstone is in the memtable
        assert_eq!(db.get(b"a").unwrap(), None);
        assert_eq!(db.scan(None, None).unwrap(), vec![kv("b", "1")]);

        db.put(b"c", b"1").unwrap(); // the tombstone is now in a newer SSTable
        assert_eq!(files(dir.path(), "sst").len(), 3);
        assert_eq!(db.get(b"a").unwrap(), None);
        assert_eq!(
            db.scan(None, None).unwrap(),
            vec![kv("b", "1"), kv("c", "1")]
        );
    }
    let db = Db::open_with(dir.path(), flush_every_write()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), None);
    assert_eq!(
        db.scan(None, None).unwrap(),
        vec![kv("b", "1"), kv("c", "1")]
    );

    // And a newer put shadows the tombstone.
    db.put(b"a", b"2").unwrap();
    db.put(b"d", b"1").unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn scan_merges_memtable_and_multiple_sstables() {
    let dir = tempfile::tempdir().unwrap();
    let check = |db: &Db| {
        let all = vec![kv("a", "2"), kv("b", "1"), kv("d", "1"), kv("e", "2")];
        assert_eq!(db.scan(None, None).unwrap(), all);
        assert_eq!(
            db.scan(Some(b"b"), Some(b"e")).unwrap(),
            vec![kv("b", "1"), kv("d", "1")]
        );
        assert_eq!(
            db.scan(Some(b"c"), None).unwrap(),
            vec![kv("d", "1"), kv("e", "2")]
        );
        assert_eq!(db.scan(None, Some(b"b")).unwrap(), vec![kv("a", "2")]);
        assert_eq!(db.scan(Some(b"c"), Some(b"d")).unwrap(), vec![]);
        assert_eq!(db.scan(Some(b"e"), Some(b"a")).unwrap(), vec![]);
    };
    {
        let db = Db::open_with(dir.path(), flush_every_write()).unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"c", b"1").unwrap();
        db.put(b"e", b"1").unwrap();
        db.put(b"b", b"1").unwrap();
        db.put(b"a", b"2").unwrap(); // shadows a=1 from an older SSTable
        db.delete(b"c").unwrap(); // shadows c=1 from an older SSTable
        db.put(b"d", b"1").unwrap();
        db.put(b"e", b"2").unwrap(); // in the memtable, shadowing e=1 on disk
        assert_eq!(files(dir.path(), "sst").len(), 7);
        check(&db);
    }
    check(&Db::open(dir.path()).unwrap());
}

/// Random puts and deletes, with frequent flushes and occasional reopens, checked against
/// a `BTreeMap`.
#[test]
fn matches_a_model_across_flushes_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rand = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let key = |i: u64| format!("key{i:03}").into_bytes();

    for round in 0..4 {
        let options = Options {
            sync_writes: false, // a clean close loses nothing, and per-write fsyncs are slow
            ..with_threshold(1000)
        };
        let db = Db::open_with(dir.path(), options).unwrap();
        for i in 0..800 {
            let k = key(rand(80));
            if rand(4) == 0 {
                db.delete(&k).unwrap();
                model.remove(&k);
            } else {
                let v = format!("r{round}-{i}-{}", "x".repeat(rand(40) as usize)).into_bytes();
                db.put(&k, &v).unwrap();
                model.insert(k, v);
            }
        }
        for i in 0..85 {
            assert_eq!(db.get(&key(i)).unwrap().as_ref(), model.get(&key(i)));
        }
        for _ in 0..20 {
            let (start, end) = (key(rand(85)), key(rand(85)));
            let expected: Vec<_> = model
                .iter()
                .filter(|(k, _)| **k >= start && **k < end)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(db.scan(Some(&start), Some(&end)).unwrap(), expected);
        }
        let all: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(db.scan(None, None).unwrap(), all);
    }
    assert!(files(dir.path(), "sst").len() > 10);
}

/// Leaves `dir` with `000001.sst` holding a=1, c=3 and a tombstone for b, and
/// `000002.log` holding d=4. Returns the contents `000001.log` had before the flush.
fn flush_and_keep_old_log(dir: &Path) -> Vec<u8> {
    {
        let db = Db::open(dir).unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
        db.put(b"c", b"3").unwrap();
        db.delete(b"b").unwrap();
    }
    let old_log = fs::read(dir.join("000001.log")).unwrap();
    {
        let db = Db::open_with(dir, flush_every_write()).unwrap();
        db.put(b"d", b"4").unwrap();
    }
    assert_eq!(files(dir, "sst"), vec!["000001.sst"]);
    assert_eq!(files(dir, "log"), vec!["000002.log"]);
    old_log
}

#[test]
fn crash_after_sstable_write_before_log_reset() {
    let dir = tempfile::tempdir().unwrap();
    let old_log = flush_and_keep_old_log(dir.path());
    // The SSTable is in place, but the crash came before the next log was created and
    // the old one deleted.
    fs::remove_file(dir.path().join("000002.log")).unwrap();
    fs::write(dir.path().join("000001.log"), &old_log).unwrap();

    let db = Db::open(dir.path()).unwrap();
    assert_eq!(
        db.scan(None, None).unwrap(),
        vec![kv("a", "1"), kv("c", "3")]
    );
    // The old log was recognized as flushed and dropped rather than replayed, so the
    // memtable starts empty and nothing is flushed a second time.
    assert_eq!(files(dir.path(), "log"), vec!["000002.log"]);
    assert_eq!(
        fs::metadata(dir.path().join("000002.log")).unwrap().len(),
        0
    );
    assert_eq!(files(dir.path(), "sst"), vec!["000001.sst"]);

    db.put(b"e", b"5").unwrap();
    drop(db);
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(
        db.scan(None, None).unwrap(),
        vec![kv("a", "1"), kv("c", "3"), kv("e", "5")]
    );
}

#[test]
fn crash_after_new_log_before_old_log_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let old_log = flush_and_keep_old_log(dir.path());
    fs::write(dir.path().join("000001.log"), &old_log).unwrap();

    let db = Db::open(dir.path()).unwrap();
    assert_eq!(
        db.scan(None, None).unwrap(),
        vec![kv("a", "1"), kv("c", "3"), kv("d", "4")]
    );
    assert_eq!(files(dir.path(), "log"), vec!["000002.log"]);
}

#[test]
fn crash_during_sstable_write_keeps_data_in_log() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path()).unwrap();
        db.put(b"a", b"1").unwrap();
    }
    // A half-written SSTable from an interrupted flush.
    fs::write(dir.path().join("000001.sst.tmp"), b"partial").unwrap();

    let db = Db::open(dir.path()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert!(files(dir.path(), "tmp").is_empty());
}

#[test]
fn legacy_wal_file_is_picked_up() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path()).unwrap();
        db.put(b"a", b"1").unwrap();
    }
    fs::rename(dir.path().join("000001.log"), dir.path().join("wal.log")).unwrap();

    let db = Db::open(dir.path()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(files(dir.path(), "log"), vec!["000001.log"]);
}

#[test]
fn stats_count_reads_and_bytes_written() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path(), flush_every_write()).unwrap();
    assert_eq!(db.stats(), Default::default());

    // Each log record is an 8-byte header, 9 bytes of kind and lengths, then key and value.
    db.put(b"a", b"1").unwrap();
    db.put(b"b", b"22").unwrap(); // flushes "a"
    db.delete(b"c").unwrap(); // flushes "b"
    let stats = db.stats();
    assert_eq!(stats.wal_bytes_written, (17 + 2) + (17 + 3) + (17 + 1));
    assert_eq!(stats.flushes, 2);
    let sst_bytes: u64 = files(dir.path(), "sst")
        .iter()
        .map(|name| fs::metadata(dir.path().join(name)).unwrap().len())
        .sum();
    assert_eq!(stats.sstable_bytes_written, sst_bytes);
    assert_eq!((stats.sstables_checked, stats.blocks_read), (0, 0));

    // "c" is a tombstone in the memtable: no SSTable is consulted.
    assert_eq!(db.get(b"c").unwrap(), None);
    assert_eq!(db.stats().sstables_checked, 0);
    // "a" is in the older SSTable. The newer one's only block ends at "b", so it could
    // hold "a" and is read too.
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
    let stats = db.stats();
    assert_eq!((stats.sstables_checked, stats.blocks_read), (2, 2));
    // Nothing is past "b", so a lookup there checks both tables but reads no block.
    assert_eq!(db.get(b"z").unwrap(), None);
    let stats = db.stats();
    assert_eq!((stats.sstables_checked, stats.blocks_read), (4, 2));
    // A scan reads every block of every table.
    assert_eq!(db.scan(None, None).unwrap().len(), 2);
    assert_eq!(db.stats().blocks_read, 4);
}
