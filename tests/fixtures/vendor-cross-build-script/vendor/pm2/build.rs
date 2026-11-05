use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build/probe.rs");
    println!("cargo:rerun-if-env-changed=RUSTC_BOOTSTRAP");
    println!("cargo:rustc-check-cfg=cfg(pm2_probe_ok)");

    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();
    fs::write(out.join("target.txt"), &target).unwrap();

    let mut cmd = Command::new(env::var_os("RUSTC").unwrap());
    cmd.args(["--edition=2021", "--crate-name=pm2_probe", "--crate-type=lib"])
        .args(["--emit=metadata", "--cap-lints=allow", "--out-dir"])
        .arg(&out)
        .arg("build/probe.rs");
    if env::var("HOST").unwrap() != target {
        cmd.arg("--target").arg(&target);
    }
    if let Ok(flags) = env::var("CARGO_ENCODED_RUSTFLAGS") {
        if !flags.is_empty() {
            cmd.args(flags.split('\x1f'));
        }
    }
    if cmd.status().map(|s| s.success()).unwrap_or(false) {
        println!("cargo:rustc-cfg=pm2_probe_ok");
    }
}
