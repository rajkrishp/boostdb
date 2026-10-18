//! Interactive shell for poking at a boostdb database.
//!
//! Usage: `cargo run -- [data-dir]` (defaults to `./boostdb-data`).

use std::io::{self, BufRead, Write};

use boostdb::Db;

const HELP: &str = "commands:
  put <key> <value>     store a value (the value may contain spaces)
  get <key>             print a value
  del <key>             delete a key
  scan [start] [end]    list keys in [start, end)
  help                  show this message
  exit                  quit";

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "boostdb-data".to_string());
    let db = match Db::open(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: cannot open {path}: {e}");
            std::process::exit(1);
        }
    };
    println!("boostdb {} — data dir: {path}", env!("CARGO_PKG_VERSION"));
    println!("type 'help' for commands");

    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        print!("boostdb> ");
        io::stdout().flush().ok();
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("error: {e}");
                break;
            }
        }
        match run(&db, line.trim()) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => println!("error: {e}"),
        }
    }
}

/// Runs one command. Returns `Ok(false)` when the shell should exit.
fn run(db: &Db, line: &str) -> boostdb::Result<bool> {
    let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
    let rest = rest.trim();
    match cmd {
        "" => {}
        "put" => match rest.split_once(' ') {
            Some((key, value)) => {
                db.put(key.as_bytes(), value.trim_start().as_bytes())?;
                println!("OK");
            }
            None => println!("usage: put <key> <value>"),
        },
        "get" if !rest.is_empty() => match db.get(rest.as_bytes())? {
            Some(value) => println!("{}", String::from_utf8_lossy(&value)),
            None => println!("(not found)"),
        },
        "get" => println!("usage: get <key>"),
        "del" if !rest.is_empty() => {
            db.delete(rest.as_bytes())?;
            println!("OK");
        }
        "del" => println!("usage: del <key>"),
        "scan" => {
            let mut bounds = rest.split_whitespace().map(str::as_bytes);
            let entries = db.scan(bounds.next(), bounds.next())?;
            for (key, value) in &entries {
                println!(
                    "{} = {}",
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(value)
                );
            }
            println!("({} entries)", entries.len());
        }
        "help" => println!("{HELP}"),
        "exit" | "quit" => return Ok(false),
        _ => println!("unknown command '{cmd}'; type 'help'"),
    }
    Ok(true)
}
