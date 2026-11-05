use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=probe.txt");
    assert!(env::var_os("CARGO_FEATURE_EXTRA").is_some(), "feature env missing");

    // Relative to the working directory, which is the package directory.
    let flavor = fs::read_to_string("probe.txt").unwrap();
    let flavor = flavor.trim();
    println!("cargo::rustc-check-cfg=cfg(probe_new)");
    println!("cargo::rustc-check-cfg=cfg(probe_core_ok)");
    println!("cargo::rustc-check-cfg=cfg(probe_flavor, values(any()))");
    println!("cargo::rustc-cfg=probe_new");
    println!("cargo::rustc-cfg=probe_flavor=\"{flavor}\"");
    println!("cargo::rustc-env=PROBE_GREETING=hi");
    println!("cargo::metadata=key=42");
    println!("cargo:warning=probe ran");

    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();
    fs::write(out.join("gen.rs"), "pub const GEN: u32 = 7;\n").unwrap();
    fs::write(out.join("target.txt"), &target).unwrap();

    // Compile a `no_std` file for the target, as autocfg-style probes
    // do. It needs the sysroot from CARGO_ENCODED_RUSTFLAGS.
    let probe = out.join("probe.rs");
    fs::write(&probe, "#![no_std]\npub fn f() {}\n").unwrap();
    let mut cmd = Command::new(env::var_os("RUSTC").unwrap());
    cmd.args(["--edition=2021", "--crate-name=probe", "--crate-type=lib"])
        .args(["--emit=metadata", "--target", &target, "--out-dir"])
        .arg(&out)
        .arg(&probe);
    let flags = env::var("CARGO_ENCODED_RUSTFLAGS").unwrap();
    if !flags.is_empty() {
        cmd.args(flags.split('\x1f'));
    }
    if cmd.status().map(|s| s.success()).unwrap_or(false) {
        println!("cargo::rustc-cfg=probe_core_ok");
    }
}
