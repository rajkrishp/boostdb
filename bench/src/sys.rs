//! Measurements taken from outside the database: memory, disk usage, machine details.

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use crate::json::Value;

/// The peak resident set size of this process so far, in bytes.
#[cfg(all(unix, target_pointer_width = "64"))]
pub fn max_rss_bytes() -> Option<u64> {
    // `struct rusage` on 64-bit macOS and Linux: two timevals, then 14 longs.
    #[repr(C)]
    struct Rusage {
        times: [i64; 4],
        maxrss: i64,
        rest: [i64; 13],
    }
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }
    const RUSAGE_SELF: i32 = 0;
    let mut usage = Rusage {
        times: [0; 4],
        maxrss: 0,
        rest: [0; 13],
    };
    // SAFETY: `usage` matches the C layout and outlives the call.
    if unsafe { getrusage(RUSAGE_SELF, &mut usage) } != 0 {
        return None;
    }
    let maxrss = u64::try_from(usage.maxrss).ok()?;
    // macOS reports bytes; Linux and the BSDs report kilobytes.
    Some(if cfg!(target_os = "macos") {
        maxrss
    } else {
        maxrss * 1024
    })
}

#[cfg(not(all(unix, target_pointer_width = "64")))]
pub fn max_rss_bytes() -> Option<u64> {
    None
}

/// Total length in bytes of the regular files in `dir` (recursively), and how many there are.
pub fn dir_usage(dir: &Path) -> io::Result<(u64, u64)> {
    let (mut bytes, mut files) = (0, 0);
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            let (b, f) = dir_usage(&entry.path())?;
            bytes += b;
            files += f;
        } else if meta.is_file() {
            bytes += meta.len();
            files += 1;
        }
    }
    Ok((bytes, files))
}

pub fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

pub fn remove_dir(dir: &Path) {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            panic!("cannot remove {}: {e}", dir.display())
        }
        _ => {}
    }
}

/// The trimmed stdout of a command, if it ran and succeeded.
pub fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

pub fn machine_info() -> Value {
    let (cpu, memory, os_version) = if cfg!(target_os = "macos") {
        (
            command_output("sysctl", &["-n", "machdep.cpu.brand_string"]),
            command_output("sysctl", &["-n", "hw.memsize"]).and_then(|m| m.parse().ok()),
            command_output("sw_vers", &["-productVersion"]),
        )
    } else {
        let field = |file: &str, name: &str| {
            let text = fs::read_to_string(file).ok()?;
            let line = text.lines().find(|l| l.starts_with(name))?;
            Some(line.split_once(':')?.1.trim().to_string())
        };
        (
            field("/proc/cpuinfo", "model name"),
            field("/proc/meminfo", "MemTotal")
                .and_then(|m| m.trim_end_matches("kB").trim().parse::<u64>().ok())
                .map(|kb| kb * 1024),
            command_output("uname", &["-r"]),
        )
    };
    let cpus = std::thread::available_parallelism()
        .ok()
        .map(|n| n.get() as u64);
    Value::Obj(vec![
        ("os".into(), std::env::consts::OS.into()),
        ("os_version".into(), os_version.into()),
        ("arch".into(), std::env::consts::ARCH.into()),
        ("cpu".into(), cpu.into()),
        ("logical_cpus".into(), cpus.into()),
        ("memory_bytes".into(), memory.into()),
        (
            "rustc".into(),
            Some(env!("BENCH_RUSTC_VERSION"))
                .filter(|v| !v.is_empty())
                .into(),
        ),
    ])
}
