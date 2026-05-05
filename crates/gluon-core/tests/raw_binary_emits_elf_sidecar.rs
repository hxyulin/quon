//! End-to-end: `gluon build` of a RawBinary crate produces both
//! `<name>.bin` (flat, the build-model primary artifact) and
//! `<name>.elf` (sidecar with debug info) in `cross_final_dir`.
//!
//! The `.elf` exists because rustc emits it directly; the `.bin` exists
//! because a post-link `rust-objcopy -O binary` step runs after rustc
//! returns success. This test exercises the whole pipeline end-to-end —
//! the unit tests under `compile/objcopy.rs::tests` cover the runner
//! in isolation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/gluon-core has a two-level parent")
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst dir");
    for entry in fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy file");
        }
    }
}

fn stage_fixture(tmp: &Path) -> PathBuf {
    let src = fixture_dir("raw-binary-minimal");
    let dst = tmp.join("raw-binary-minimal");
    copy_dir_recursive(&src, &dst);
    dst
}

#[test]
#[ignore]
fn raw_binary_emits_elf_sidecar_with_debug_info() {
    if gluon_core::RustcInfo::probe().is_err() {
        eprintln!("raw_binary_emits_elf_sidecar_with_debug_info: skipped — rustc probe failed");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = stage_fixture(tmp.path());

    // Load + resolve.
    let model = gluon_core::load_build_model(&project).expect("load_build_model");
    let resolved = gluon_core::resolve_config(&model, "dev", None, &project, None)
        .expect("resolve_config");

    // Build context: layout under `<project>/build`, freshly probed
    // RustcInfo, empty cache.
    let layout = gluon_core::compile::BuildLayout::new(project.join("build"), "raw-binary-minimal");
    let rustc_info = gluon_core::compile::RustcInfo::load_or_probe(&layout)
        .expect("RustcInfo probe");
    let cache = gluon_core::cache::Cache::load(layout.cache_manifest()).0;
    let ctx = gluon_core::CompileCtx::new(layout, Arc::new(rustc_info), cache);

    // Run the build. Any error (sysroot compile, rustc spawn, objcopy)
    // will surface here and fail the test with a precise message.
    let _summary = gluon_core::build(&ctx, &model, &resolved).expect("gluon build");

    // After the build, both files must live in cross_final_dir.
    // Path: build/cross/x86_64-unknown-none/dev/final/{boot.bin, boot.elf}
    let final_dir = project
        .join("build/cross")
        .join("x86_64-unknown-none")
        .join("dev")
        .join("final");
    let bin = final_dir.join("boot.bin");
    let elf = final_dir.join("boot.elf");

    assert!(bin.is_file(), ".bin must exist at {}", bin.display());
    assert!(elf.is_file(), ".elf must exist at {}", elf.display());

    // ELF magic check — confirms it really is an ELF, not a flat blob
    // mislabelled.
    let elf_bytes = fs::read(&elf).expect("read elf");
    assert!(
        elf_bytes.len() >= 4,
        ".elf is suspiciously small ({} bytes)",
        elf_bytes.len()
    );
    assert_eq!(
        &elf_bytes[..4],
        &[0x7f, 0x45, 0x4c, 0x46],
        "ELF magic missing — sidecar is not a real ELF"
    );

    // Sanity: the .elf must NOT be byte-identical to the .bin. If they
    // matched, --oformat=binary leaked back somehow (or objcopy did
    // nothing).
    let bin_bytes = fs::read(&bin).expect("read bin");
    assert_ne!(
        elf_bytes, bin_bytes,
        ".elf and .bin must differ — they were identical, suggesting the rustc step still emits flat binary"
    );
}
