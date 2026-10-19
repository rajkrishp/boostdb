//! Benchmark harness for boostdb: produces the same scorecard for every tutorial part.
//!
//! It uses only the public API every part has (`Db::open_with`, `put`, `get`, `delete`,
//! `scan`), plus `Db::stats()` behind the `stats` feature, so it compiles against old tags.
//!
//! ```text
//! boostdb-bench [run] [--keys N] [--reps N] [--out FILE] ...   run every workload, print JSON
//! boostdb-bench summary DIR                                    compare DIR/*.json as Markdown
//! ```
//!
//! `run` does each workload in a child process (`boostdb-bench child <workload> ...`) so
//! that peak memory is the workload's alone and the crash check can SIGKILL a writer.

mod child;
mod json;
mod run;
mod summary;
mod sys;
mod workload;

use std::collections::HashMap;
use std::process;
use std::str::FromStr;

const USAGE: &str = "usage:
  boostdb-bench [run] [options]     run the workloads and print the JSON scorecard
  boostdb-bench summary DIR         print a Markdown table comparing DIR/*.json

run options (defaults in brackets):
  --keys N             keys loaded (16-byte keys, 100-byte values) [1000000]
  --reps N             runs of each timed workload; the median is reported [5]
  --read-ops N         point reads per run, half hits and half misses [40000]
  --scan-ops N         range scans per run [1000]
  --scan-len N         expected keys per range scan [100]
  --sync-ops N         writes per run of the sync_writes = true workload [500]
  --crash-rounds N     SIGKILLs in the crash-durability check [20]
  --crash-value-len N  value size in the crash check, large enough to force flushes [16384]
  --seed N             seed for every random choice [42]
  --work-dir DIR       where the databases go (deleted afterwards) [a new temp dir]
  --out FILE           write the JSON here instead of stdout
  --tag, --commit, --subject, --run-at   recorded in the JSON (run_all.sh sets them)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("child") => child::main(&args[1..]),
        Some("summary") => summary::main(&args[1..]),
        Some("run") => run::main(&Args::parse(&args[1..])),
        Some("-h" | "--help" | "help") => println!("{USAGE}"),
        Some(arg) if !arg.starts_with("--") => fail(&format!("unknown command {arg:?}")),
        _ => run::main(&Args::parse(&args)),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("boostdb-bench: {message}\n\n{USAGE}");
    process::exit(2);
}

/// `--name value` pairs.
pub struct Args(HashMap<String, String>);

impl Args {
    pub fn parse(args: &[String]) -> Args {
        let mut map = HashMap::new();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let Some(name) = arg.strip_prefix("--") else {
                fail(&format!("unexpected argument {arg:?}"));
            };
            let Some(value) = it.next() else {
                fail(&format!("--{name} needs a value"));
            };
            map.insert(name.to_string(), value.clone());
        }
        Args(map)
    }

    pub fn str(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn get<T: FromStr>(&self, name: &str, default: T) -> T {
        match self.str(name) {
            None => default,
            Some(v) => v
                .replace('_', "")
                .parse()
                .unwrap_or_else(|_| fail(&format!("bad value for --{name}: {v:?}"))),
        }
    }

    pub fn required(&self, name: &str) -> &str {
        self.str(name)
            .unwrap_or_else(|| fail(&format!("missing --{name}")))
    }
}
