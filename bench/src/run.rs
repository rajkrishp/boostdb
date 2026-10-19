//! The orchestrator: runs each workload in child processes and assembles the scorecard.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use boostdb::{Db, Options};

use crate::child::CRASH_VERSION;
use crate::json::Value;
use crate::workload::{KEY_LEN, Rng, VALUE_LEN, key, value};
use crate::{Args, fail, sys};

/// Bump when a metric is renamed, removed or changes meaning (adding one is fine).
const SCHEMA_VERSION: u64 = 1;

struct Config {
    keys: u64,
    reps: usize,
    read_ops: u64,
    scan_ops: u64,
    scan_len: u64,
    sync_ops: u64,
    crash_rounds: u64,
    crash_value_len: u64,
    seed: u64,
}

/// One child's `name value` results.
type Report = BTreeMap<String, f64>;

pub fn main(args: &Args) {
    let cfg = Config {
        keys: args.get("keys", 1_000_000),
        reps: args.get("reps", 5),
        read_ops: args.get("read-ops", 40_000),
        scan_ops: args.get("scan-ops", 1_000),
        scan_len: args.get("scan-len", 100),
        sync_ops: args.get("sync-ops", 500),
        crash_rounds: args.get("crash-rounds", 20),
        crash_value_len: args.get("crash-value-len", 16 * 1024),
        seed: args.get("seed", 42),
    };
    if cfg.keys == 0 || cfg.reps == 0 {
        fail("--keys and --reps must be at least 1");
    }
    let work = args.str("work-dir").map_or_else(
        || std::env::temp_dir().join(format!("boostdb-bench-{}", std::process::id())),
        PathBuf::from,
    );
    sys::remove_dir(&work);
    fs::create_dir_all(&work).unwrap();
    let metrics = measure(&cfg, &work);
    sys::remove_dir(&work);

    let commit = args.str("commit").map(str::to_string).or_else(|| {
        let repo = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
        sys::command_output("git", &["-C", repo, "rev-parse", "HEAD"])
    });
    let doc = Value::Obj(vec![
        ("schema_version".into(), SCHEMA_VERSION.into()),
        ("tag".into(), args.str("tag").into()),
        ("commit".into(), commit.into()),
        ("subject".into(), args.str("subject").into()),
        ("run_at".into(), args.str("run-at").into()),
        ("machine".into(), sys::machine_info()),
        (
            "config".into(),
            Value::Obj(vec![
                ("keys".into(), cfg.keys.into()),
                ("key_bytes".into(), (KEY_LEN as u64).into()),
                ("value_bytes".into(), (VALUE_LEN as u64).into()),
                ("reps".into(), (cfg.reps as u64).into()),
                ("read_ops".into(), cfg.read_ops.into()),
                ("scan_ops".into(), cfg.scan_ops.into()),
                ("scan_len".into(), cfg.scan_len.into()),
                ("sync_ops".into(), cfg.sync_ops.into()),
                ("crash_rounds".into(), cfg.crash_rounds.into()),
                ("crash_value_bytes".into(), cfg.crash_value_len.into()),
                ("seed".into(), cfg.seed.into()),
                ("stats_available".into(), cfg!(feature = "stats").into()),
            ]),
        ),
        ("metrics".into(), Value::Obj(metrics)),
    ]);
    let text = doc.to_pretty();
    match args.str("out") {
        Some(path) => fs::write(path, text).unwrap_or_else(|e| panic!("cannot write {path}: {e}")),
        None => print!("{text}"),
    }
}

fn measure(cfg: &Config, work: &Path) -> Vec<(String, Value)> {
    let n = cfg.keys;
    let entry_bytes = (KEY_LEN + VALUE_LEN) as u64;
    let base = work.join("base");
    let scratch = work.join("scratch");
    let mut m = Metrics(Vec::new());
    let mut wrong_results = 0.0;

    progress(&format!("load {n} keys, sync_writes off"));
    let loads = repeat(cfg.reps, |rep| {
        // Each run starts empty; the last one is kept for the workloads that follow.
        let dir = if rep + 1 == cfg.reps { &base } else { &scratch };
        sys::remove_dir(dir);
        child("load", dir, &[("n", n.to_string())])
    });
    sys::remove_dir(&scratch);
    m.put("load_ops_per_sec", ops_per_sec(n, &loads));
    m.put("peak_rss_load_bytes", median(&loads, "max_rss_bytes"));
    m.put("write_amp_load", write_amp(&loads, n * entry_bytes));
    let live = n * entry_bytes;
    let (disk, files) = sys::dir_usage(&base).unwrap();
    m.put("live_bytes_after_load", live);
    m.put("disk_bytes_after_load", disk);
    m.put("space_amp_after_load", disk as f64 / live as f64);
    m.put("files_after_load", files);

    progress(&format!("{} writes, sync_writes on", cfg.sync_ops));
    let syncs = repeat(cfg.reps, |_| {
        sys::remove_dir(&scratch);
        let args = [("n", cfg.sync_ops.to_string()), ("sync", "true".into())];
        child("load", &scratch, &args)
    });
    sys::remove_dir(&scratch);
    m.put("sync_write_ops_per_sec", ops_per_sec(cfg.sync_ops, &syncs));

    progress("recovery: open after the load");
    let opens = repeat(cfg.reps, |_| child("open", &base, &[]));
    m.put("recovery_ms", median(&opens, "open_ns").map(|ns| ns / 1e6));
    m.put("peak_rss_open_bytes", median(&opens, "max_rss_bytes"));

    progress(&format!("{} point reads, half misses", cfg.read_ops));
    let read_args = |verify: bool| {
        [
            ("n", n.to_string()),
            ("ops", cfg.read_ops.to_string()),
            ("seed", cfg.seed.to_string()),
            ("verify", verify.to_string()),
        ]
    };
    let reads = repeat(cfg.reps, |_| child("reads", &base, &read_args(true)));
    wrong_results += sum(&reads, "wrong_results");
    for (kind, label) in [("hit", "get_hit"), ("miss", "get_miss")] {
        m.put(
            &format!("{label}_p50_us"),
            micros(&reads, &format!("{kind}_p50_ns")),
        );
        m.put(
            &format!("{label}_p99_us"),
            micros(&reads, &format!("{kind}_p99_ns")),
        );
    }
    for (kind, counter, name) in [
        ("hit", "sstables_checked", "sstables_per_get_hit"),
        ("miss", "sstables_checked", "sstables_per_get_miss"),
        ("hit", "blocks_read", "blocks_per_get_hit"),
        ("miss", "blocks_read", "blocks_per_get_miss"),
    ] {
        let count = format!("{kind}_count");
        m.put(
            name,
            ratio(&reads, &format!("{kind}_stats.{counter}"), &count),
        );
    }

    progress(&format!(
        "{} range scans of ~{} keys",
        cfg.scan_ops, cfg.scan_len
    ));
    let scans = repeat(cfg.reps, |_| {
        let args = [
            ("n", n.to_string()),
            ("ops", cfg.scan_ops.to_string()),
            ("len", cfg.scan_len.to_string()),
            ("seed", cfg.seed.to_string()),
        ];
        child("scans", &base, &args)
    });
    m.put("scan_p50_us", micros(&scans, "scan_p50_ns"));
    m.put("scan_p99_us", micros(&scans, "scan_p99_ns"));
    m.put("scan_rows_avg", ratio(&scans, "rows", "scan_count"));
    m.put(
        "blocks_per_scan",
        ratio(&scans, "stats.blocks_read", "scan_count"),
    );

    progress(&format!("overwrite: {n} writes to random existing keys"));
    let overwrites = repeat(cfg.reps, |_| {
        sys::remove_dir(&scratch);
        sys::copy_dir(&base, &scratch).unwrap();
        let args = [
            ("n", n.to_string()),
            ("ops", n.to_string()),
            ("seed", cfg.seed.to_string()),
        ];
        child("overwrite", &scratch, &args)
    });
    m.put("overwrite_ops_per_sec", ops_per_sec(n, &overwrites));
    m.put(
        "write_amp_overwrite",
        write_amp(&overwrites, n * entry_bytes),
    );
    let (disk, files) = sys::dir_usage(&scratch).unwrap();
    m.put("disk_bytes_after_overwrite", disk);
    m.put("space_amp_after_overwrite", disk as f64 / live as f64);
    m.put("files_after_overwrite", files);
    let reads = repeat(cfg.reps, |_| child("reads", &scratch, &read_args(false)));
    wrong_results += sum(&reads, "wrong_results");
    m.put(
        "get_hit_after_overwrite_p50_us",
        micros(&reads, "hit_p50_ns"),
    );
    m.put(
        "get_hit_after_overwrite_p99_us",
        micros(&reads, "hit_p99_ns"),
    );
    let name = "sstables_per_get_hit_after_overwrite";
    m.put(
        name,
        ratio(&reads, "hit_stats.sstables_checked", "hit_count"),
    );
    sys::remove_dir(&scratch);

    progress("delete-heavy: delete half the keys");
    let deletes = repeat(cfg.reps, |_| {
        sys::remove_dir(&scratch);
        sys::copy_dir(&base, &scratch).unwrap();
        child("deletes", &scratch, &[("n", n.to_string())])
    });
    wrong_results += sum(&deletes, "wrong_results");
    let deleted = median(&deletes, "deleted").unwrap_or(0.0) as u64;
    m.put("delete_ops_per_sec", ops_per_sec(deleted, &deletes));
    let live = (n - deleted) * entry_bytes;
    let (disk, files) = sys::dir_usage(&scratch).unwrap();
    m.put("live_bytes_after_delete", live);
    m.put("disk_bytes_after_delete", disk);
    m.put("space_amp_after_delete", disk as f64 / live.max(1) as f64);
    m.put("files_after_delete", files);
    sys::remove_dir(&scratch);

    progress(&format!(
        "crash check: {} SIGKILLs during sync writes",
        cfg.crash_rounds
    ));
    let crash = crash_check(cfg, &work.join("crash"));
    m.put("crash_rounds", cfg.crash_rounds);
    m.put("acked_writes", crash.acked);
    m.put("acked_writes_lost", crash.lost);
    m.put("crash_reopen_failures", crash.reopen_failures);

    m.put("wrong_results", wrong_results);
    m.0
}

struct Metrics(Vec<(String, Value)>);

impl Metrics {
    fn put(&mut self, name: &str, value: impl Into<Value>) {
        self.0.push((name.to_string(), value.into()));
    }
}

struct CrashResult {
    acked: u64,
    lost: u64,
    reopen_failures: u64,
}

/// Repeatedly starts a writer with `sync_writes` on, SIGKILLs it at a random moment, reopens
/// the database, and checks that every write the writer reported as acknowledged is there.
fn crash_check(cfg: &Config, dir: &Path) -> CrashResult {
    let mut rng = Rng::new(cfg.seed ^ 0xc4a5);
    let mut acked = Vec::new();
    let mut lost = BTreeSet::new();
    let mut reopen_failures = 0;
    let mut next = 0;
    for _ in 0..cfg.crash_rounds {
        let mut writer = Command::new(std::env::current_exe().unwrap())
            .args(["child", "crash-writer", "--dir"])
            .arg(dir)
            .args(["--start", &next.to_string()])
            .args(["--value-len", &cfg.crash_value_len.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("cannot start the crash writer");
        let mut stdout = writer.stdout.take().unwrap();
        let reader = thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            text
        });
        thread::sleep(Duration::from_millis(5 + rng.below(250)));
        writer.kill().unwrap(); // SIGKILL
        writer.wait().unwrap();
        let text = reader.join().unwrap();
        // A line cut off by the kill was never fully reported, so it doesn't count.
        let lines = text.split_inclusive('\n').filter(|l| l.ends_with('\n'));
        acked.extend(lines.map(|l| l.trim().parse::<u64>().unwrap()));
        if let Some(&last) = acked.last() {
            next = last + 1;
        }

        let options = Options {
            sync_writes: false,
            ..Options::default()
        };
        let db = match Db::open_with(dir, options) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("[bench] reopen after SIGKILL failed: {e}");
                reopen_failures += 1;
                continue;
            }
        };
        let len = cfg.crash_value_len as usize;
        for &i in &acked {
            if db.get(&key(i)).ok().flatten() != Some(value(i, CRASH_VERSION, len)) {
                lost.insert(i);
            }
        }
    }
    sys::remove_dir(dir);
    CrashResult {
        acked: acked.len() as u64,
        lost: lost.len() as u64,
        reopen_failures,
    }
}

fn progress(what: &str) {
    eprintln!("[bench] {what}");
}

/// Runs `workload` in a child process and parses its results.
fn child(workload: &str, dir: &Path, args: &[(&str, String)]) -> Report {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["child", workload, "--dir"]).arg(dir);
    for (name, value) in args {
        cmd.arg(format!("--{name}")).arg(value);
    }
    let out = cmd.stderr(Stdio::inherit()).output().unwrap();
    assert!(
        out.status.success(),
        "workload {workload} failed: {}",
        out.status
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let (name, value) = line.split_once(' ').expect("malformed child output");
            (
                name.to_string(),
                value.parse().expect("malformed child output"),
            )
        })
        .collect()
}

fn repeat(reps: usize, mut f: impl FnMut(usize) -> Report) -> Vec<Report> {
    (0..reps).map(&mut f).collect()
}

/// The median of a result over all runs, if every run reported it.
fn median(runs: &[Report], name: &str) -> Option<f64> {
    median_of(
        runs.iter()
            .map(|r| r.get(name).copied())
            .collect::<Option<_>>()?,
    )
}

fn median_of(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(f64::total_cmp);
    let mid = xs.len() / 2;
    Some(if xs.len() % 2 == 1 {
        xs[mid]
    } else {
        (xs[mid - 1] + xs[mid]) / 2.0
    })
}

/// The median over runs of `numerator / denominator`.
fn ratio(runs: &[Report], numerator: &str, denominator: &str) -> Option<f64> {
    let per_run = runs
        .iter()
        .map(|r| Some(r.get(numerator)? / r.get(denominator)?))
        .collect::<Option<Vec<_>>>()?;
    median_of(per_run.into_iter().filter(|x| x.is_finite()).collect())
}

fn sum(runs: &[Report], name: &str) -> f64 {
    runs.iter().filter_map(|r| r.get(name)).sum()
}

fn micros(runs: &[Report], name: &str) -> Option<f64> {
    median(runs, name).map(|ns| ns / 1e3)
}

fn ops_per_sec(ops: u64, runs: &[Report]) -> Option<f64> {
    let per_run = runs
        .iter()
        .map(|r| Some(ops as f64 / (r.get("elapsed_ns")? / 1e9)))
        .collect::<Option<Vec<_>>>()?;
    median_of(per_run)
}

/// Bytes the database wrote (logs plus SSTables) per byte of keys and values written.
fn write_amp(runs: &[Report], user_bytes: u64) -> Option<f64> {
    let per_run = runs
        .iter()
        .map(|r| {
            let written =
                r.get("stats.wal_bytes_written")? + r.get("stats.sstable_bytes_written")?;
            Some(written / user_bytes as f64)
        })
        .collect::<Option<Vec<_>>>()?;
    median_of(per_run)
}
