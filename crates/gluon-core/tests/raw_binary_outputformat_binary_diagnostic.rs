//! Negative-case integration test: a RawBinary crate whose linker
//! script contains `OUTPUT_FORMAT(binary)` must fail the build with a
//! specific diagnostic that points the user at the offending directive.
//!
//! The unit-level test in `compile/objcopy.rs::tests::
//! run_objcopy_rejects_non_elf_input_with_outputformat_hint` covers the
//! magic-byte pre-check in isolation. This test confirms the diagnostic
//! propagates through `gluon_core::build` to the caller — i.e. the
//! end-user actually sees the install hint when their linker script is
//! the problem.

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
    let src = fixture_dir("raw-binary-bad-linker");
    let dst = tmp.join("raw-binary-bad-linker");
    copy_dir_recursive(&src, &dst);
    dst
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
    let project = stage_fixture(tmp.path());

    let model = gluon_core::load_build_model(&project).expect("load_build_model");
    let resolved =
        gluon_core::resolve_config(&model, "dev", None, &project, None).expect("resolve_config");

    // Build context: layout under `<project>/build`, freshly probed
    // RustcInfo, empty cache.
    let layout =
        gluon_core::compile::BuildLayout::new(project.join("build"), "raw-binary-bad-linker");
    let rustc_info =
        gluon_core::compile::RustcInfo::load_or_probe(&layout).expect("RustcInfo probe");
    let ctx = gluon_core::CompileCtx::new(layout, Arc::new(rustc_info));

    // Build must fail because the post-link objcopy step's magic-byte
    // pre-check rejects the supposed `.elf` (which is actually a flat
    // binary because the linker script forced OUTPUT_FORMAT(binary)).
    let err = gluon_core::build(&ctx, &model, &resolved)
        .expect_err("build must fail when linker script forces OUTPUT_FORMAT(binary)");
    let msg = format!("{err}");
    assert!(
        msg.contains("OUTPUT_FORMAT(binary)"),
        "diagnostic must mention OUTPUT_FORMAT(binary), got:\n{msg}"
    );
}
