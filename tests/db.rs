use std::fs::OpenOptions;
use std::io::Write;

use boostdb::{Db, Error, MAX_KEY_SIZE};

fn kv(k: &str, v: &str) -> (Vec<u8>, Vec<u8>) {
    (k.as_bytes().to_vec(), v.as_bytes().to_vec())
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
    let wal = dir.path().join("wal.log");
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
    let wal = dir.path().join("wal.log");
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
