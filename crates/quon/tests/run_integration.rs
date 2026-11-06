//! End-to-end CLI integration tests for `quon run`.
//!
//! These tests drive the real `quon` binary against the `minimal`
//! and `minimal-uefi` fixtures in `--dry-run` mode. Dry-run skips
//! both the build step and the QEMU spawn, returning only the
//! assembled argv to stdout — so these tests:
//!
//! - do **not** require a working rustc toolchain,
//! - do **not** require QEMU to be installed,
//! - do **not** require a real OVMF firmware (we point at tempfile
//!   stubs via `OVMF_CODE` / `OVMF_VARS`),
//!
//! and therefore run in the default test set without an `#[ignore]`
//! gate. A separate `spawn_real_qemu_smoke` test (gated behind the
//! `QUON_TEST_QEMU=1` env var *and* `#[ignore]`) exercises the
//! actual spawn path on a dev box.

mod support;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use support::{rustc_available, seed_sysroot, setup_fixture};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn quon_command(fixture: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_quon"));
    cmd.current_dir(fixture);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Scrub inherited OVMF env vars so tests are hermetic by default.
    // Individual tests that need them will re-add them explicitly.
    cmd.env_remove("OVMF_CODE");
    cmd.env_remove("OVMF_VARS");
    cmd
}

/// Parse a `quon run --dry-run` stdout line into `(binary, args)`.
///
/// The dry-run printer emits a single shell-quoted line:
/// `qemu-system-x86_64 -machine q35 -m 256M ...`. This helper
/// splits on whitespace which is good enough for our tests because
/// all paths in the fixtures are alphanumeric + `-`/`_`/`.`/`/`.
fn parse_dry_run(stdout: &str) -> (String, Vec<String>) {
    let line = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .expect("at least one non-empty stdout line");
    let mut parts = line.split_whitespace();
    let binary = parts.next().expect("binary").to_string();
    let args: Vec<String> = parts.map(|s| s.to_string()).collect();
    (binary, args)
}

fn contains_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn quon_run_direct_dry_run_against_minimal() {
    let (_tmp, fixture) = setup_fixture("minimal");

    let output = quon_command(&fixture)
        .args(["run", "--dry-run"])
        .output()
        .expect("spawn quon run");

    assert!(
        output.status.success(),
        "quon run --dry-run failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (binary, args) = parse_dry_run(&stdout);
    assert_eq!(binary, "qemu-system-x86_64");
    assert!(
        contains_pair(&args, "-machine", "q35"),
        "expected -machine q35, got {args:?}"
    );
    assert!(
        contains_pair(&args, "-m", "256M"),
        "expected -m 256M, got {args:?}"
    );
    assert!(
        contains_pair(&args, "-smp", "1"),
        "expected -smp 1, got {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "-kernel"),
        "expected direct-mode -kernel flag, got {args:?}"
    );
    // Direct mode must NOT emit any pflash drives.
    assert!(
        !args.iter().any(|a| a.contains("pflash")),
        "direct mode must not emit pflash drives, got {args:?}"
    );
    // The run binary path must end in cross/.../dev/final/kernel.
    let kernel_idx = args.iter().position(|a| a == "-kernel").unwrap();
    let kernel_path = &args[kernel_idx + 1];
    assert!(
        kernel_path.ends_with("cross/x86_64-unknown-none/dev/final/kernel"),
        "kernel path looks wrong: {kernel_path}"
    );
}

#[test]
fn quon_run_uefi_dry_run_against_minimal_uefi() {
    let (_tmp, fixture) = setup_fixture("minimal-uefi");

    // Materialise fake OVMF files inside the tempdir so the resolver
    // can validate their existence. We only care that the argv points
    // at them; we never actually read their bytes.
    let code = fixture.join("OVMF_CODE.fd");
    let vars = fixture.join("OVMF_VARS.fd");
    fs::write(&code, b"fake code").unwrap();
    fs::write(&vars, b"fake vars").unwrap();

    let output = quon_command(&fixture)
        .env("OVMF_CODE", &code)
        .env("OVMF_VARS", &vars)
        .args(["run", "--dry-run"])
        .output()
        .expect("spawn quon run --uefi-from-profile");

    assert!(
        output.status.success(),
        "quon run --dry-run (uefi) failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, args) = parse_dry_run(&stdout);

    // Two pflash drives, readonly code + writable vars.
    let pflash: Vec<_> = args
        .windows(2)
        .filter(|w| w[0] == "-drive" && w[1].contains("if=pflash"))
        .map(|w| w[1].clone())
        .collect();
    assert_eq!(pflash.len(), 2, "expected 2 pflash drives, got {pflash:?}");
    assert!(
        pflash.iter().any(|d| d.contains("readonly=on")),
        "one pflash drive must be readonly, got {pflash:?}"
    );
    assert!(
        pflash
            .iter()
            .any(|d| !d.contains("readonly=on") && d.contains("format=raw")),
        "one pflash drive must be writable, got {pflash:?}"
    );

    // ESP dir mounted via VVFAT.
    assert!(
        args.iter().any(|a| a.contains("fat:rw:")),
        "expected fat:rw: ESP drive, got {args:?}"
    );

    // No direct -kernel flag in UEFI mode.
    assert!(
        !args.iter().any(|a| a == "-kernel"),
        "UEFI mode must not emit -kernel, got {args:?}"
    );

    // Profile-level memory (512) overrode the default.
    assert!(
        contains_pair(&args, "-m", "512M"),
        "expected profile's -m 512M, got {args:?}"
    );
    assert!(
        contains_pair(&args, "-smp", "2"),
        "expected profile's -smp 2, got {args:?}"
    );
}

#[test]
fn quon_run_cli_uefi_flag_overrides_direct_profile() {
    // Start from the `minimal` fixture (direct mode) and force UEFI
    // via the CLI flag, supplying OVMF via env vars. Should produce
    // a UEFI argv even though the fixture's QEMU runner never
    // called .boot_mode("uefi").
    let (_tmp, fixture) = setup_fixture("minimal");
    let code = fixture.join("OVMF_CODE.fd");
    let vars = fixture.join("OVMF_VARS.fd");
    fs::write(&code, b"fake code").unwrap();
    fs::write(&vars, b"fake vars").unwrap();

    let output = quon_command(&fixture)
        .env("OVMF_CODE", &code)
        .env("OVMF_VARS", &vars)
        .args(["run", "--uefi", "--dry-run"])
        .output()
        .expect("spawn quon run --uefi");

    assert!(
        output.status.success(),
        "quon run --uefi --dry-run failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, args) = parse_dry_run(&stdout);
    assert!(
        args.iter().any(|a| a.contains("if=pflash")),
        "expected pflash drives after --uefi, got {args:?}"
    );
    assert!(
        !args.iter().any(|a| a == "-kernel"),
        "CLI --uefi should suppress -kernel, got {args:?}"
    );
}

#[test]
fn quon_run_without_runner_errors_clearly() {
    // The `minimal` fixture's dev profile names the `qemu` runner; drop
    // that and confirm `quon run` rejects the profile with the
    // `NoRunner` diagnostic. `--dry-run` keeps the test hermetic, and
    // would still catch a regression that moved the check past the
    // build step.
    let (_tmp, fixture) = setup_fixture("minimal");
    strip_profile_runner(&fixture);

    let output = quon_command(&fixture)
        .args(["run", "--dry-run"])
        .output()
        .expect("spawn quon run");

    assert!(
        !output.status.success(),
        "quon run should fail when the profile has no runner, status={:?}, stdout=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("profile 'dev' has no runner"),
        "expected NoRunner wording in stderr, got: {stderr}"
    );
}

#[test]
fn quon_run_uefi_without_ovmf_errors_loudly() {
    // `minimal-uefi` declares boot_mode uefi but its qemu() block
    // does not set explicit ovmf paths. With env vars scrubbed
    // (quon_command already does this) and no system OVMF, the
    // resolver should fail with the three-layer diagnostic.
    //
    // On a developer box with system OVMF installed, the resolver
    // might actually succeed — in that case we expect the argv path,
    // not the error. We detect that case and skip the error-path
    // assertion.

    let (_tmp, fixture) = setup_fixture("minimal-uefi");

    let output = quon_command(&fixture)
        .args(["run", "--dry-run"])
        .output()
        .expect("spawn quon run");

    if output.status.success() {
        // System OVMF was found — confirm the argv is a UEFI one and move on.
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("if=pflash"),
            "system OVMF was found but argv is not UEFI: {stdout}"
        );
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no OVMF firmware found"),
        "stderr should describe the OVMF resolver failure, got: {stderr}"
    );
    // All three fallback layers should be mentioned in the diagnostic.
    assert!(
        stderr.contains("Explicit") && stderr.contains("Env") && stderr.contains("System paths"),
        "resolver diagnostic should list all three fallback layers, got: {stderr}"
    );
}

#[test]
fn quon_run_gdb_appends_dash_s_upper_s() {
    // `--gdb` must inject `-s -S` into the argv. We verify via the
    // dry-run path so the test is hermetic (no QEMU spawn). The two
    // flags should appear *after* the managed quon section (machine,
    // memory, kernel) but before any user-provided `-- extras`.
    let (_tmp, fixture) = setup_fixture("minimal");

    let output = quon_command(&fixture)
        .args(["run", "--gdb", "--dry-run"])
        .output()
        .expect("spawn quon run --gdb --dry-run");

    assert!(
        output.status.success(),
        "quon run --gdb --dry-run failed: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, args) = parse_dry_run(&stdout);

    assert!(
        args.iter().any(|a| a == "-s"),
        "expected -s in argv, got {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "-S"),
        "expected -S in argv, got {args:?}"
    );
    // -s must come after -kernel (the managed section); this protects
    // the invariant that user-visible quon flags don't break the
    // "managed first, user overrides second" layout.
    let pos_kernel = args.iter().position(|a| a == "-kernel").unwrap();
    let pos_s = args.iter().position(|a| a == "-s").unwrap();
    assert!(pos_kernel < pos_s, "-s should follow -kernel, got {args:?}");
}

#[test]
fn quon_run_extra_args_pass_through() {
    let (_tmp, fixture) = setup_fixture("minimal");

    let output = quon_command(&fixture)
        .args(["run", "--dry-run", "--", "-no-reboot", "-d", "int"])
        .output()
        .expect("spawn quon run -- ...");

    assert!(
        output.status.success(),
        "quon run --dry-run -- ... failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, args) = parse_dry_run(&stdout);
    // Extra args appear after the managed quon section.
    let pos_no_reboot = args.iter().position(|a| a == "-no-reboot").unwrap();
    let pos_d = args.iter().position(|a| a == "-d").unwrap();
    let pos_kernel = args.iter().position(|a| a == "-kernel").unwrap();
    assert!(pos_kernel < pos_no_reboot && pos_no_reboot < pos_d);
    assert!(args.iter().any(|a| a == "int"));
}

// ---------------------------------------------------------------------------
// --no-build path
// ---------------------------------------------------------------------------

/// Write a stand-in for `qemu-system-*` into `dir` and return its path.
/// It records its arguments, one per line, to `$FAKE_QEMU_ARGV_FILE`
/// when that is set, and exits 0. A script keeps it out of the
/// binaries `cargo install` ships.
#[cfg(unix)]
fn write_fake_qemu(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-qemu");
    fs::write(
        &path,
        "#!/bin/sh\n[ -n \"$FAKE_QEMU_ARGV_FILE\" ] && printf '%s\\n' \"$@\" > \"$FAKE_QEMU_ARGV_FILE\"\nexit 0\n",
    )
    .expect("write fake-qemu");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake-qemu");
    path.to_str().expect("utf-8 temp path").to_owned()
}

#[cfg(unix)]
/// Rewrite the fixture's `qemu("...")` binary to point at a different path.
fn rewrite_qemu_binary(fixture: &Path, new_binary: &str) {
    let rs_path = fixture.join("quon.rs");
    let original = fs::read_to_string(&rs_path).expect("read quon.rs");
    let rewritten = original.replace("qemu-system-x86_64", new_binary);
    assert_ne!(
        original, rewritten,
        "expected fixture to contain 'qemu-system-x86_64' literal so the \
         test can rewrite it; check tests/fixtures/minimal/quon.rs"
    );
    fs::write(&rs_path, rewritten).expect("write quon.rs");
}

/// Drop `.runner("qemu")` from the fixture's dev profile, leaving the
/// runner declared but unused, so the profile has no runner.
fn strip_profile_runner(fixture: &Path) {
    let rs_path = fixture.join("quon.rs");
    let original = fs::read_to_string(&rs_path).expect("read quon.rs");
    let rewritten = original.replacen("        .runner(\"qemu\")\n", "", 1);
    assert_ne!(
        original, rewritten,
        "expected the fixture's profile to name the qemu runner"
    );
    fs::write(&rs_path, rewritten).expect("write quon.rs");
}

#[cfg(unix)]
#[test]
fn quon_run_no_build_skips_build_and_passes_kernel_path() {
    // `--no-build` must skip the build, so nothing is compiled under
    // build/cross/, and still hand QEMU the would-be run binary path:
    // a regression could skip the build *and* the `-kernel` argument.
    let (_tmp, fixture) = setup_fixture("minimal");
    let fake_qemu = write_fake_qemu(&fixture);
    rewrite_qemu_binary(&fixture, &fake_qemu);

    let argv_file = fixture.join("fake-qemu-argv.txt");

    let output = quon_command(&fixture)
        .env("FAKE_QEMU_ARGV_FILE", &argv_file)
        .args(["run", "--no-build"])
        .output()
        .expect("spawn quon run --no-build");

    assert!(
        output.status.success(),
        "quon run --no-build failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let cross_dir = fixture.join("build").join("cross");
    assert!(
        !cross_dir.exists(),
        "build/cross/ should not exist after --no-build, but found {cross_dir:?}"
    );

    let argv = fs::read_to_string(&argv_file).expect("fake-qemu wrote argv");
    let lines: Vec<&str> = argv.lines().collect();
    let kernel_idx = lines
        .iter()
        .position(|l| *l == "-kernel")
        .unwrap_or_else(|| panic!("expected -kernel in fake-qemu argv:\n{argv}"));
    let kernel_path = lines
        .get(kernel_idx + 1)
        .expect("kernel path follows -kernel");
    assert!(
        kernel_path.ends_with("cross/x86_64-unknown-none/dev/final/kernel"),
        "kernel path looks wrong: {kernel_path}"
    );
}

// ---------------------------------------------------------------------------
// uefi-bootloader-kernel fixture — Step 6 of the UEFI end-to-end work.
// Exercises the artifact_env + auto-ESP + QEMU auto-wire chain against
// a two-target project (x86_64-unknown-uefi bootloader embedding an
// x86_64-unknown-none kernel).
// ---------------------------------------------------------------------------

#[test]
fn quon_run_uefi_bootloader_kernel_dry_run_emits_auto_esp() {
    // With no explicit `qemu().esp_dir(...)` and exactly one
    // `esp("default")` declaration, dry-run must emit the path-arithmetic
    // ESP directory (build/cross/x86_64-unknown-uefi/dev/esp/default)
    // as the fat:rw: drive target.
    let (_tmp, fixture) = setup_fixture("uefi-bootloader-kernel");
    let code = fixture.join("OVMF_CODE.fd");
    let vars = fixture.join("OVMF_VARS.fd");
    fs::write(&code, b"fake code").unwrap();
    fs::write(&vars, b"fake vars").unwrap();

    let output = quon_command(&fixture)
        .env("OVMF_CODE", &code)
        .env("OVMF_VARS", &vars)
        .args(["run", "--dry-run"])
        .output()
        .expect("spawn quon run --dry-run");

    assert!(
        output.status.success(),
        "quon run --dry-run failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, args) = parse_dry_run(&stdout);

    // Pflash drives present: UEFI mode is wired.
    let pflash_count = args
        .windows(2)
        .filter(|w| w[0] == "-drive" && w[1].contains("if=pflash"))
        .count();
    assert_eq!(
        pflash_count, 2,
        "expected 2 pflash drives in UEFI mode, got {args:?}"
    );

    // Auto-wired ESP: -drive format=raw,file=fat:rw:<esp_dir>
    let fat_drives: Vec<_> = args
        .windows(2)
        .filter(|w| w[0] == "-drive" && w[1].contains("fat:rw:"))
        .map(|w| w[1].clone())
        .collect();
    assert_eq!(
        fat_drives.len(),
        1,
        "expected exactly one fat:rw: drive (auto-wired ESP), got {args:?}"
    );

    // The path must resolve to the build-output ESP directory under
    // the profile's cross target. We don't require the directory to
    // exist — dry-run skips the build, so it won't.
    let fat_drive = &fat_drives[0];
    let expected_suffix = "build/cross/x86_64-unknown-uefi/dev/esp/default";
    assert!(
        fat_drive.contains(expected_suffix),
        "auto-wired ESP path should end in '{expected_suffix}', got: {fat_drive}"
    );

    // No direct -kernel: UEFI mode routes through OVMF, not QEMU's
    // direct loader.
    assert!(
        !args.iter().any(|a| a == "-kernel"),
        "UEFI mode must not emit -kernel, got {args:?}"
    );
}

#[test]
#[ignore]
fn quon_build_uefi_bootloader_kernel_assembles_esp() {
    // Full build path: compile both the kernel (x86_64-unknown-none)
    // and the bootloader (x86_64-unknown-uefi), run the EspBuild node,
    // and verify the on-disk ESP layout.
    //
    // Gated by #[ignore] + real-rustc probe because it needs
    // `rust-src` for both targets. Run with:
    //   cargo test -p quon --test run_integration -- --ignored \
    //     quon_build_uefi_bootloader_kernel_assembles_esp
    if !rustc_available("quon_build_uefi_bootloader_kernel_assembles_esp") {
        return;
    }

    let (_tmp, fixture) = setup_fixture("uefi-bootloader-kernel");
    seed_sysroot(&fixture);

    let output = quon_command(&fixture)
        .args(["build"])
        .output()
        .expect("spawn quon build");

    assert!(
        output.status.success(),
        "quon build failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let esp_dir = fixture
        .join("build")
        .join("cross")
        .join("x86_64-unknown-uefi")
        .join("dev")
        .join("esp")
        .join("default");
    assert!(
        esp_dir.is_dir(),
        "ESP directory should exist at {esp_dir:?}"
    );

    let boot_x64 = esp_dir.join("EFI").join("BOOT").join("BOOTX64.EFI");
    assert!(
        boot_x64.is_file(),
        "bootloader should have been copied to {boot_x64:?}"
    );
    let boot_bytes = fs::read(&boot_x64).expect("read BOOTX64.EFI");
    assert!(!boot_bytes.is_empty(), "BOOTX64.EFI must not be empty");
    // PE32+ header magic: MZ at offset 0.
    assert_eq!(
        &boot_bytes[..2],
        b"MZ",
        "BOOTX64.EFI should be a PE binary (starts with MZ), got first 2 bytes: {:?}",
        &boot_bytes[..2]
    );

    // The kernel ELF should NOT be in the ESP (it's embedded in the
    // bootloader via include_bytes!, not shipped as a separate file).
    let esp_kernel_elf = esp_dir.join("kernel");
    assert!(
        !esp_kernel_elf.exists(),
        "kernel must not appear in ESP — the bootloader embeds it"
    );

    // Sanity: the standalone kernel ELF should exist at its own path
    // (proves the cross-group build actually ran both targets).
    let kernel_elf = fixture
        .join("build")
        .join("cross")
        .join("x86_64-unknown-none")
        .join("dev")
        .join("final")
        .join("kernel");
    assert!(
        kernel_elf.is_file(),
        "kernel ELF should exist at {kernel_elf:?}"
    );
}

#[test]
#[ignore]
fn quon_run_uefi_bootloader_kernel_real_qemu_debugcon() {
    // Full spawn path: builds + boots the bootloader under QEMU/OVMF
    // and asserts on debugcon output. Gated by QUON_TEST_QEMU=1
    // because it needs both a real rustc toolchain AND a working
    // qemu-system-x86_64 binary with OVMF firmware discoverable.
    if std::env::var("QUON_TEST_QEMU").ok().as_deref() != Some("1") {
        eprintln!(
            "quon_run_uefi_bootloader_kernel_real_qemu_debugcon: \
             skipped — set QUON_TEST_QEMU=1 to enable"
        );
        return;
    }
    if !rustc_available("quon_run_uefi_bootloader_kernel_real_qemu_debugcon") {
        return;
    }

    let (_tmp, fixture) = setup_fixture("uefi-bootloader-kernel");
    seed_sysroot(&fixture);
    let debugcon_path = fixture.join("debugcon.out");
    // Best-effort: create an empty file the bootloader can append to.
    fs::write(&debugcon_path, b"").ok();

    let debugcon_arg = format!("file:{}", debugcon_path.display());

    let output = quon_command(&fixture)
        .args([
            "run",
            "-T",
            "10",
            "--",
            "-debugcon",
            &debugcon_arg,
            "-display",
            "none",
            "-no-reboot",
        ])
        .output()
        .expect("spawn quon run");

    // The bootloader halts in an hlt loop, so we expect the timeout
    // to fire. Both a clean exit (unlikely) and a timeout-induced
    // non-zero status are acceptable; what matters is the captured
    // debugcon output.
    let _ = output;

    let captured = fs::read(&debugcon_path).unwrap_or_default();
    assert!(
        captured.starts_with(b"K"),
        "debugcon must start with 'K' (kernel non-empty marker), got: {:?}",
        String::from_utf8_lossy(&captured)
    );
}

// --- Real QEMU smoke test (opt-in) ---

/// Actually spawn QEMU against a direct-mode kernel ELF. Gated behind
/// `QUON_TEST_QEMU=1` *and* `#[ignore]` because it needs both `rustc`
/// (to build the kernel) and `qemu-system-x86_64` installed.
#[test]
#[ignore]
fn spawn_real_qemu_smoke() {
    if std::env::var("QUON_TEST_QEMU").ok().as_deref() != Some("1") {
        eprintln!("spawn_real_qemu_smoke: skipped — set QUON_TEST_QEMU=1 to enable");
        return;
    }
    if quon_core::RustcInfo::probe().is_err() {
        eprintln!("spawn_real_qemu_smoke: skipped — rustc probe failed");
        return;
    }
    let (_tmp, fixture) = setup_fixture("minimal");
    seed_sysroot(&fixture);

    let output = quon_command(&fixture)
        .args(["run", "-T", "5"])
        .output()
        .expect("spawn quon run");

    // The stub kernel's PVH ELF note allows QEMU >= 7.2 to direct-boot
    // it. The kernel loops forever, so the timeout should fire and quon
    // reports QemuTimeout as a non-zero exit. A QEMU load error is also
    // acceptable (e.g. if the PVH note doesn't work on a particular QEMU
    // build) — the test's purpose is to verify that quon's runner
    // correctly spawns QEMU and reports the result.
    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let is_timeout = stderr.to_lowercase().contains("timeout");
    let is_qemu_load_error = stderr.contains("runner 'qemu' exited with status");
    assert!(
        is_timeout || is_qemu_load_error,
        "expected either a timeout or a QEMU load error in stderr, got: {stderr}"
    );
}

/// Point the fixture's dev profile at a command runner declared next to
/// its QEMU runner.
fn use_command_runner(fixture: &Path, argv: &str) {
    let rs_path = fixture.join("quon.rs");
    let original = fs::read_to_string(&rs_path).expect("read quon.rs");
    let rewritten = original
        .replacen(
            "        .runner(\"qemu\")\n",
            "        .runner(\"board\")\n",
            1,
        )
        .replacen(
            "    cfg.runner(\"qemu\")",
            &format!("    cfg.runner(\"board\").command(&{argv});\n    cfg.runner(\"qemu\")"),
            1,
        );
    assert_ne!(original, rewritten, "fixture shape drifted");
    fs::write(&rs_path, rewritten).expect("write quon.rs");
}

/// A command runner runs its argv with `{binary}` filled in and the
/// arguments after `--` appended. QEMU-only flags are refused.
#[test]
fn command_runner_runs_its_argv_with_the_binary() {
    let (_tmp, fixture) = setup_fixture("minimal");
    use_command_runner(&fixture, r#"["flash", "--image", "{binary}"]"#);

    let output = quon_command(&fixture)
        .args(["run", "--dry-run", "--", "--speed", "4000"])
        .output()
        .expect("spawn quon run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(line.starts_with("flash --image /"), "{line}");
    assert!(line.ends_with("/kernel --speed 4000"), "{line}");

    for flag in ["--gdb", "--uefi"] {
        let output = quon_command(&fixture)
            .args(["run", "--dry-run", flag])
            .output()
            .expect("spawn quon run");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{flag} should fail");
        assert!(
            stderr.contains("runner 'board' is a command runner")
                && stderr.contains("needs a QEMU runner"),
            "{flag}: {stderr}"
        );
    }
}

/// `quon run` judges a command runner by its exit status and its
/// `success_exit_code`.
#[test]
#[ignore]
#[cfg(unix)]
fn command_runner_exit_status_decides_the_run() {
    if !rustc_available("command_runner_exit_status_decides_the_run") {
        return;
    }
    let (_tmp, fixture) = setup_fixture("minimal");
    seed_sysroot(&fixture);
    use_command_runner(
        &fixture,
        // Exits 7 when the built binary exists, so a run that never
        // built it, or passed the wrong path, fails differently.
        r#"["sh", "-c", "test -f \"$0\" && exit 7", "{binary}"]"#,
    );
    let output = quon_command(&fixture)
        .args(["run"])
        .output()
        .expect("spawn quon run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("runner 'board' exited with status 7"),
        "{stderr}"
    );

    // Exit status 7 is the success status once the runner says so.
    let rs_path = fixture.join("quon.rs");
    let text = fs::read_to_string(&rs_path).unwrap();
    fs::write(
        &rs_path,
        text.replacen("\"{binary}\"]);", "\"{binary}\"]).success_exit_code(7);", 1),
    )
    .unwrap();
    let output = quon_command(&fixture)
        .args(["run", "--no-build"])
        .output()
        .expect("spawn quon run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("Exited runner 'board' with status 7"),
        "{stderr}"
    );
}

/// Point the fixture's dev profile at a runner served by the `fake`
/// plugin, declared next to its QEMU runner.
fn use_plugin_runner(fixture: &Path) {
    let rs_path = fixture.join("quon.rs");
    let original = fs::read_to_string(&rs_path).expect("read quon.rs");
    let rewritten = original
        .replacen(
            "        .runner(\"qemu\")\n",
            "        .runner(\"board\")\n",
            1,
        )
        .replacen(
            "    cfg.runner(\"qemu\")",
            "    cfg.runner(\"board\").plugin(\"fake\").setting(\"chip\", \"nrf\");\n    cfg.runner(\"qemu\")",
            1,
        );
    assert_ne!(original, rewritten, "fixture shape drifted");
    fs::write(&rs_path, rewritten).expect("write quon.rs");
}

/// Write `quon-runner-fake` into `dir`: it appends each request to
/// `$FAKE_RUNNER_LOG`, one per line, and answers `validate` with the
/// defaults and `prepare` with a fixed command.
#[cfg(unix)]
fn write_fake_runner(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("quon-runner-fake");
    fs::write(
        &path,
        r#"#!/bin/sh
req=$(cat)
printf '%s\n' "$req" >> "$FAKE_RUNNER_LOG"
case "$req" in
  *'"method":"validate"'*) printf '{"ok":{}}' ;;
  *) printf '{"ok":{"program":"flash","args":["--chip","nrf"],"env":{"FAKE":"1"}}}' ;;
esac
"#,
    )
    .expect("write quon-runner-fake");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// A runner plugin not built into quon is found on `PATH` and asked
/// over the JSON protocol: `validate` before the build, `prepare` with
/// the binary after.
#[cfg(unix)]
#[test]
fn an_external_runner_plugin_on_path_assembles_the_command() {
    let (_tmp, fixture) = setup_fixture("minimal");
    use_plugin_runner(&fixture);
    let bin = fixture.join("plugin-bin");
    fs::create_dir_all(&bin).unwrap();
    write_fake_runner(&bin);
    let log = fixture.join("fake-runner.log");
    let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();

    let output = quon_command(&fixture)
        .env("PATH", &path)
        .env("FAKE_RUNNER_LOG", &log)
        .args(["run", "--dry-run", "--uefi", "--", "--speed", "4000"])
        .output()
        .expect("spawn quon run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(line, "FAKE=1 flash --chip nrf");

    let log = fs::read_to_string(&log).expect("the plugin ran");
    let requests: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON request"))
        .collect();
    assert_eq!(requests.len(), 2, "{log}");
    let validate = &requests[0];
    assert_eq!(validate["protocol"], 1);
    assert_eq!(validate["request"]["method"], "validate");
    let params = &validate["request"]["params"];
    assert_eq!(params["runner"], "board");
    assert_eq!(params["target"], "x86_64-unknown-none");
    assert_eq!(params["settings"], serde_json::json!({"chip": "nrf"}));
    assert_eq!(
        params["overrides"],
        serde_json::json!({"boot_mode": "uefi"})
    );
    let prepare = &requests[1]["request"];
    assert_eq!(prepare["method"], "prepare");
    let params = &prepare["params"];
    assert!(
        params["binary"]
            .as_str()
            .unwrap()
            .ends_with("cross/x86_64-unknown-none/dev/final/kernel"),
        "{params}"
    );
    assert_eq!(params["extra_args"], serde_json::json!(["--speed", "4000"]));
    assert_eq!(params["built"], false);
}

/// A runner plugin that is neither built in nor on `PATH` fails before
/// the build and names the executable it looked for.
#[test]
fn a_missing_runner_plugin_names_the_executable() {
    let (_tmp, fixture) = setup_fixture("minimal");
    use_plugin_runner(&fixture);
    let output = quon_command(&fixture)
        .args(["run", "--dry-run"])
        .output()
        .expect("spawn quon run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("runner 'board' uses the 'fake' runner plugin")
            && stderr.contains("`quon-runner-fake` is not on PATH"),
        "{stderr}"
    );
}
