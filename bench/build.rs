//! Records the compiler version, so results say which rustc built the code they measured.

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = std::process::Command::new(rustc)
        .arg("-V")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .unwrap_or_default();
    println!("cargo:rustc-env=BENCH_RUSTC_VERSION={}", version.trim());
}
