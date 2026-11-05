//! End-to-end test for `gluon init`: generate a `gluon.rs` for a copy of
//! `examples/limine-kernel` with its own `gluon.rs` removed, then build
//! it with the generated file.
//!
//! `#[ignore]`-gated like the other build tests: it needs rustc with the
//! `rust-src` component, and the build reads the `limine` crate from
//! Cargo's registry cache.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name();
        if ty.is_dir() && (name == "build" || name == "esp") {
            continue;
        }
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(name))?;
        } else if ty.is_file() {
            fs::copy(entry.path(), dst.join(name))?;
        }
    }
    Ok(())
}

/// `gluon` run in `dir`. The tempdir is outside the repository, so its
/// `rust-toolchain.toml` does not apply; the child names the toolchain.
fn gluon(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_gluon"))
        .current_dir(dir)
        .env("RUSTUP_TOOLCHAIN", "nightly")
        .args(args)
        .output()
        .expect("spawn gluon");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
#[ignore]
fn init_writes_a_gluon_rs_that_builds_the_limine_example() {
    match gluon_core::RustcInfo::probe() {
        Ok(info) if info.rust_src.is_some() => {}
        other => {
            eprintln!("skipped: rustc with rust-src is unavailable ({other:?})");
            return;
        }
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("limine-kernel");
    copy_dir_all(&repo_root().join("examples/limine-kernel"), &dir).expect("copy example");
    fs::remove_file(dir.join("gluon.rs")).expect("remove gluon.rs");

    let (ok, _, stderr) = gluon(&dir, &["init", "--bootloader", "limine"]);
    assert!(ok, "gluon init failed:\n{stderr}");
    let text = fs::read_to_string(dir.join("gluon.rs")).expect("read generated gluon.rs");
    for line in [
        r#"cfg.project("limine-kernel", "0.1.0");"#,
        r#"let x86_64 = cfg.target("x86_64-unknown-none");"#,
        r#".boot_binary("kernel");"#,
        r#".linker_script("crates/kernel/kernel.ld")"#,
        r#".rustc_flag("-Crelocation-model=static");"#,
        r#".add_file("limine.conf", "limine.conf")"#,
        r#".add("kernel", "boot/kernel");"#,
        r#".boot_mode("uefi");"#,
    ] {
        assert!(text.contains(line), "missing `{line}` in:\n{text}");
    }

    // A second run refuses to overwrite the file.
    let (ok, _, stderr) = gluon(&dir, &["init"]);
    assert!(!ok && stderr.contains("--force"), "{stderr}");

    let (ok, _, stderr) = gluon(&dir, &["build"]);
    assert!(
        ok,
        "gluon build with the generated gluon.rs failed:\n{stderr}\n{text}"
    );
    assert!(
        dir.join("build/cross/x86_64-unknown-none/dev/esp/default/boot/kernel")
            .is_file(),
        "the ESP holds the kernel"
    );
    assert!(
        dir.join("build/cross/x86_64-unknown-none/dev/esp/default/limine.conf")
            .is_file()
    );
}
