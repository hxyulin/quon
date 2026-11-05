//! End-to-end builds of a RawBinary crate.
//!
//! - `tests/fixtures/raw-binary-minimal` produces both `<name>.bin`
//!   (flat, the build-model primary artifact) and `<name>.elf` (sidecar
//!   with debug info) in `cross_final_dir`. The `.elf` exists because
//!   rustc emits it directly; the `.bin` exists because a post-link
//!   `rust-objcopy -O binary` step runs after rustc returns success.
//! - `tests/fixtures/raw-binary-bad-linker` is the same project with
//!   `OUTPUT_FORMAT(binary)` in its linker script. The build must fail
//!   with a diagnostic that names the directive.
//!
//! The unit tests under `compile/objcopy.rs::tests` cover the objcopy
//! runner and its magic-byte pre-check in isolation; these confirm the
//! whole pipeline, and that the diagnostic reaches the caller of
//! `gluon_core::build`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Copy `tests/fixtures/<fixture>` into `tmp`, seed its sysroot and
/// build its `dev` profile. Returns the project and the build result.
fn build(tmp: &Path, fixture: &str) -> (PathBuf, gluon_core::Result<gluon_core::BuildSummary>) {
    let project = tmp.join(fixture);
    copy_dir_recursive(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(fixture),
        &project,
    );
    let model = gluon_core::load_build_model(&project).expect("load_build_model");
    let resolved =
        gluon_core::resolve_config(&model, "dev", None, &project, None).expect("resolve_config");
    let layout = gluon_core::compile::BuildLayout::new(project.join("build"), fixture);
    let rustc_info =
        gluon_core::compile::RustcInfo::load_or_probe(&layout).expect("RustcInfo probe");
    gluon_core::testing::seed_sysroots(
        &project.join("build"),
        &model,
        rustc_info.rustc_arg.as_ref(),
    )
    .expect("seed sysroot");
    let ctx = gluon_core::CompileCtx::new(layout, Arc::new(rustc_info));
    let result = gluon_core::build(&ctx, &model, &resolved);
    (project, result)
}

#[test]
#[ignore]
fn raw_binary_emits_elf_sidecar_with_debug_info() {
    if gluon_core::RustcInfo::probe().is_err() {
        eprintln!("raw_binary_emits_elf_sidecar_with_debug_info: skipped — rustc probe failed");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (project, result) = build(tmp.path(), "raw-binary-minimal");
    result.expect("gluon build");

    let final_dir = project.join("build/cross/x86_64-unknown-none/dev/final");
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

    // If the .elf matched the .bin, --oformat=binary leaked back
    // somehow, or objcopy did nothing.
    let bin_bytes = fs::read(&bin).expect("read bin");
    assert_ne!(
        elf_bytes, bin_bytes,
        ".elf and .bin must differ — they were identical, suggesting the rustc step still emits flat binary"
    );
}

#[test]
#[ignore]
fn output_format_binary_in_linker_script_produces_specific_error() {
    if gluon_core::RustcInfo::probe().is_err() {
        eprintln!(
            "output_format_binary_in_linker_script_produces_specific_error: \
             skipped — rustc probe failed"
        );
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    // The post-link objcopy step's magic-byte pre-check rejects the
    // supposed `.elf`, which is a flat binary because the linker script
    // forced OUTPUT_FORMAT(binary).
    let (_, result) = build(tmp.path(), "raw-binary-bad-linker");
    let err = result.expect_err("build must fail when linker script forces OUTPUT_FORMAT(binary)");
    let msg = format!("{err}");
    assert!(
        msg.contains("OUTPUT_FORMAT(binary)"),
        "diagnostic must mention OUTPUT_FORMAT(binary), got:\n{msg}"
    );
}
