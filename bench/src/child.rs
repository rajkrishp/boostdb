//! The workloads, each run as `boostdb-bench child <workload> --dir DIR ...` in a process
//! of its own. A workload prints its results to stdout as `name value` lines.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use boostdb::{Db, Options};

use crate::workload::{MISS_BASE, Rng, VALUE_LEN, deleted, hex, key, value};
use crate::{Args, fail, sys};

/// The `version` the crash check's writer uses for every value.
pub const CRASH_VERSION: u64 = 7;

pub fn main(args: &[String]) {
    let Some((workload, rest)) = args.split_first() else {
        fail("child needs a workload");
    };
    let args = Args::parse(rest);
    let dir = Path::new(args.required("dir"));
    let mut out = Output(Vec::new());
    match workload.as_str() {
        "load" => load(dir, &args, &mut out),
        "open" => open_only(dir, &mut out),
        "reads" => reads(dir, &args, &mut out),
        "scans" => scans(dir, &args, &mut out),
        "overwrite" => overwrite(dir, &args, &mut out),
        "deletes" => deletes(dir, &args, &mut out),
        "crash-writer" => crash_writer(dir, &args),
        other => fail(&format!("unknown workload {other:?}")),
    }
    if let Some(rss) = sys::max_rss_bytes() {
        out.push("max_rss_bytes", rss as f64);
    }
    let mut stdout = std::io::stdout().lock();
    for (name, value) in out.0 {
        writeln!(stdout, "{name} {value}").unwrap();
    }
}

struct Output(Vec<(String, f64)>);

impl Output {
    fn push(&mut self, name: &str, value: f64) {
        self.0.push((name.to_string(), value));
    }

    fn duration(&mut self, name: &str, d: Duration) {
        self.push(name, d.as_nanos() as f64);
    }

    /// The counters `db` gained since `before`, if this part of boostdb has them.
    fn stats(&mut self, prefix: &str, db: &Db, before: Option<Counters>) {
        if let (Some(after), Some(before)) = (counters(db), before) {
            let delta = after.minus(before);
            for (name, value) in delta.fields() {
                self.push(&format!("{prefix}{name}"), value as f64);
            }
        }
    }

    /// Nearest-rank p50 and p99 of `samples`, in nanoseconds.
    fn percentiles(&mut self, prefix: &str, samples: &mut [u64]) {
        samples.sort_unstable();
        for (name, p) in [("p50_ns", 0.50), ("p99_ns", 0.99)] {
            if !samples.is_empty() {
                let rank = ((p * samples.len() as f64).ceil() as usize).max(1);
                self.push(&format!("{prefix}{name}"), samples[rank - 1] as f64);
            }
        }
        self.push(&format!("{prefix}count"), samples.len() as f64);
    }
}

fn open(dir: &Path, sync_writes: bool) -> Db {
    // Struct update syntax keeps this compiling as Options gains fields.
    let options = Options {
        sync_writes,
        ..Options::default()
    };
    Db::open_with(dir, options).unwrap_or_else(|e| panic!("cannot open {}: {e}", dir.display()))
}

/// Writes keys `0..n` with fresh values.
fn load(dir: &Path, args: &Args, out: &mut Output) {
    let n: u64 = args.get("n", 0);
    let db = open(dir, args.get("sync", false));
    let before = counters(&db);
    let start = Instant::now();
    for i in 0..n {
        db.put(&key(i), &value(i, 0, VALUE_LEN)).unwrap();
    }
    out.duration("elapsed_ns", start.elapsed());
    out.stats("stats.", &db, before);
}

/// Just opens the database: recovery time, and the memory it takes to hold what it recovered.
fn open_only(dir: &Path, out: &mut Output) {
    let start = Instant::now();
    let db = open(dir, false);
    out.duration("open_ns", start.elapsed());
    drop(db);
}

/// Random point reads, a coin flip between a loaded key and one never written. With
/// `--verify true`, a hit must return the value `load` wrote.
fn reads(dir: &Path, args: &Args, out: &mut Output) {
    let n: u64 = args.get("n", 0);
    let ops: u64 = args.get("ops", 0);
    let verify: bool = args.get("verify", false);
    let mut rng = Rng::new(args.get("seed", 0));
    let db = open(dir, false);
    let (mut hit_ns, mut miss_ns) = (Vec::new(), Vec::new());
    let (mut hit_stats, mut miss_stats) = (Counters::default(), Counters::default());
    let mut wrong = 0;
    for _ in 0..ops {
        let hit = n > 0 && rng.next_u64() & 1 == 0;
        let i = if hit {
            rng.below(n)
        } else {
            MISS_BASE + rng.below(MISS_BASE)
        };
        let k = key(i);
        let before = counters(&db);
        let start = Instant::now();
        let got = db.get(&k).unwrap();
        let elapsed = start.elapsed().as_nanos() as u64;
        let delta = counters(&db).zip(before).map(|(a, b)| a.minus(b));
        let ok = match (&got, hit) {
            (None, _) => !hit,
            (Some(v), true) => !verify || *v == value(i, 0, VALUE_LEN),
            (Some(_), false) => false,
        };
        wrong += u64::from(!ok);
        let (samples, stats) = if hit {
            (&mut hit_ns, &mut hit_stats)
        } else {
            (&mut miss_ns, &mut miss_stats)
        };
        samples.push(elapsed);
        if let Some(delta) = delta {
            *stats = stats.plus(delta);
        }
    }
    out.percentiles("hit_", &mut hit_ns);
    out.percentiles("miss_", &mut miss_ns);
    out.push("wrong_results", wrong as f64);
    if counters(&db).is_some() {
        for (prefix, stats) in [("hit_stats.", hit_stats), ("miss_stats.", miss_stats)] {
            for (name, value) in stats.fields() {
                out.push(&format!("{prefix}{name}"), value as f64);
            }
        }
    }
}

/// Range scans over random ranges sized to hold about `--len` keys each.
fn scans(dir: &Path, args: &Args, out: &mut Output) {
    let n: u64 = args.get("n", 1);
    let ops: u64 = args.get("ops", 0);
    let span = (u64::MAX / n.max(1)).saturating_mul(args.get("len", 100));
    let mut rng = Rng::new(args.get("seed", 0));
    let db = open(dir, false);
    let before = counters(&db);
    let mut samples = Vec::new();
    let mut rows = 0;
    for _ in 0..ops {
        let lo = rng.next_u64();
        let (start, end) = (hex(lo), hex(lo.saturating_add(span)));
        let t = Instant::now();
        rows += db.scan(Some(&start), Some(&end)).unwrap().len();
        samples.push(t.elapsed().as_nanos() as u64);
    }
    out.percentiles("scan_", &mut samples);
    out.push("rows", rows as f64);
    out.stats("stats.", &db, before);
}

/// `--ops` writes of new values to random existing keys.
fn overwrite(dir: &Path, args: &Args, out: &mut Output) {
    let n: u64 = args.get("n", 0);
    let ops: u64 = args.get("ops", 0);
    let mut rng = Rng::new(args.get("seed", 0));
    let db = open(dir, false);
    let before = counters(&db);
    let start = Instant::now();
    for version in 1..=ops {
        let i = rng.below(n);
        db.put(&key(i), &value(i, version, VALUE_LEN)).unwrap();
    }
    out.duration("elapsed_ns", start.elapsed());
    out.stats("stats.", &db, before);
}

/// Deletes a fixed pseudo-random half of keys `0..n`, then spot-checks the result.
fn deletes(dir: &Path, args: &Args, out: &mut Output) {
    let n: u64 = args.get("n", 0);
    let db = open(dir, false);
    let before = counters(&db);
    let start = Instant::now();
    let mut count = 0;
    for i in (0..n).filter(|&i| deleted(i)) {
        db.delete(&key(i)).unwrap();
        count += 1;
    }
    out.duration("elapsed_ns", start.elapsed());
    out.push("deleted", count as f64);
    out.stats("stats.", &db, before);
    let wrong = (0..n.min(2000))
        .filter(|&i| {
            let expected = (!deleted(i)).then(|| value(i, 0, VALUE_LEN));
            db.get(&key(i)).unwrap() != expected
        })
        .count();
    out.push("wrong_results", wrong as f64);
}

/// Writes keys `--start`, `--start + 1`, ... with `sync_writes` on, printing each index
/// once its `put` has returned `Ok`, until it is killed.
fn crash_writer(dir: &Path, args: &Args) -> ! {
    let len: usize = args.get("value-len", VALUE_LEN);
    let db = open(dir, true);
    let mut stdout = std::io::stdout().lock();
    for i in args.get("start", 0u64).. {
        db.put(&key(i), &value(i, CRASH_VERSION, len)).unwrap();
        // If the parent has gone away, so should we.
        if writeln!(stdout, "{i}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            std::process::exit(1);
        }
    }
    unreachable!()
}

/// `boostdb::Stats`, copied so the code compiles against parts that lack it.
#[derive(Debug, Clone, Copy, Default)]
struct Counters {
    sstables_checked: u64,
    blocks_read: u64,
    wal_bytes_written: u64,
    sstable_bytes_written: u64,
    flushes: u64,
}

impl Counters {
    fn fields(self) -> [(&'static str, u64); 5] {
        [
            ("sstables_checked", self.sstables_checked),
            ("blocks_read", self.blocks_read),
            ("wal_bytes_written", self.wal_bytes_written),
            ("sstable_bytes_written", self.sstable_bytes_written),
            ("flushes", self.flushes),
        ]
    }

    fn zip_with(self, other: Counters, f: impl Fn(u64, u64) -> u64) -> Counters {
        Counters {
            sstables_checked: f(self.sstables_checked, other.sstables_checked),
            blocks_read: f(self.blocks_read, other.blocks_read),
            wal_bytes_written: f(self.wal_bytes_written, other.wal_bytes_written),
            sstable_bytes_written: f(self.sstable_bytes_written, other.sstable_bytes_written),
            flushes: f(self.flushes, other.flushes),
        }
    }

    fn minus(self, other: Counters) -> Counters {
        self.zip_with(other, u64::wrapping_sub)
    }

    fn plus(self, other: Counters) -> Counters {
        self.zip_with(other, u64::wrapping_add)
    }
}

#[cfg(feature = "stats")]
fn counters(db: &Db) -> Option<Counters> {
    let s = db.stats();
    Some(Counters {
        sstables_checked: s.sstables_checked,
        blocks_read: s.blocks_read,
        wal_bytes_written: s.wal_bytes_written,
        sstable_bytes_written: s.sstable_bytes_written,
        flushes: s.flushes,
    })
}

#[cfg(not(feature = "stats"))]
fn counters(_: &Db) -> Option<Counters> {
    None
}
