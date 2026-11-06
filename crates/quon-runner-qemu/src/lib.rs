//! The QEMU runner plugin: boots a built binary under QEMU.
//!
//! Quon builds this runner into the `quon` binary as the `qemu`
//! plugin, and the `quon-runner-qemu` executable serves the same code
//! over the runner protocol (see [`quon_runner`]).
//!
//! Its settings are the fields of [`quon_model::QemuDef`], which
//! `Qemu::new(program)` fills in. The overrides are the
//! profile's `qemu_memory`, `qemu_cores`, `qemu_extra_args` and
//! `qemu_boot_mode`, and a boot mode flag from the command line (see
//! [`resolve::Overrides`]).
//!
//! Layering:
//!
//! - [`qemu_cmd`] assembles the QEMU argv as a pure function, with the
//!   per-boot-mode parts in [`boot_mode`].
//! - [`ovmf`] probes for OVMF firmware files (explicit, then env, then
//!   beside QEMU, then system paths) and makes the writable vars copy.
//! - [`resolve`] merges the settings and overrides into a
//!   fully-defaulted [`ResolvedQemu`].
//! - [`QemuRunner`] wires those together behind the [`Runner`] trait.

pub mod boot_mode;
pub mod error;
pub mod ovmf;
pub mod qemu_cmd;
pub mod resolve;

pub use error::{Error, Result};
pub use ovmf::{OvmfResolveCtx, ResolvedOvmf, resolve_ovmf};
pub use qemu_cmd::{QemuCommand, build_qemu_command};
pub use resolve::{Overrides, ResolvedQemu, resolve_disk_source, resolve_qemu};

use quon_model::{BootMode, EspSource, QemuDef};
use quon_runner::{
    ArtifactKind, Context, PrepareRequest, PrepareResponse, Runner, RunnerError, TestBinary,
    ValidateResponse,
};
use std::collections::BTreeMap;
use std::ffi::OsString;

/// The QEMU runner.
#[derive(Debug, Clone, Copy, Default)]
pub struct QemuRunner;

impl Runner for QemuRunner {
    fn validate(&self, context: &Context) -> std::result::Result<ValidateResponse, RunnerError> {
        let (def, resolved) = parse(context)?;
        // A test binary replaces the run binary in whatever the
        // firmware boots: the `-kernel` argument for direct boot, the
        // ESP for UEFI.
        let test_binary = match resolved.boot_mode {
            BootMode::Direct => TestBinary::AsBinary,
            BootMode::Uefi => TestBinary::EspCopy,
            BootMode::Bios => TestBinary::Unsupported {
                reason: "quon test boots each test through UEFI or direct boot; BIOS boot \
                         reads a disk image quon cannot rebuild per test"
                    .into(),
            },
        };
        Ok(ValidateResponse {
            test_binary,
            test_timeout: def.test_timeout.map(u64::from),
        })
    }

    fn prepare(
        &self,
        request: &PrepareRequest,
    ) -> std::result::Result<PrepareResponse, RunnerError> {
        Ok(prepare(request)?)
    }
}

/// Read the settings and overrides and resolve them for the target.
fn parse(context: &Context) -> Result<(QemuDef, ResolvedQemu)> {
    let def: QemuDef = from_map(&context.settings).map_err(|e| {
        Error::Config(format!(
            "runner '{}': invalid QEMU settings: {e}",
            context.runner
        ))
    })?;
    let overrides: Overrides = from_map(&context.overrides).map_err(|e| {
        Error::Config(format!(
            "runner '{}': invalid QEMU override: {e}",
            context.runner
        ))
    })?;
    // For a built-in target the spec is the triple
    // (`x86_64-unknown-none`); for a custom JSON spec it is the spec
    // path, which users typically name with the arch prefix, so the
    // arch-prefix match in `default_binary_for_target` still has a
    // reasonable shot.
    let triple = context.target.as_deref().ok_or_else(|| {
        Error::Config(format!(
            "profile '{}' builds for the host, and booting under QEMU needs a target; \
             use a profile with `.target(..)`",
            context.profile
        ))
    })?;
    let resolved = resolve_qemu(&def, &overrides, triple)?;
    Ok((def, resolved))
}

fn from_map<T: serde::de::DeserializeOwned>(
    map: &BTreeMap<String, serde_json::Value>,
) -> serde_json::Result<T> {
    let object = map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    serde_json::from_value(serde_json::Value::Object(object))
}

/// Assemble the QEMU command: resolve the disk, the ESP and OVMF, then
/// the argv.
fn prepare(request: &PrepareRequest) -> Result<PrepareResponse> {
    let context = &request.context;
    let (def, mut resolved) = parse(context)?;

    // `Qemu::disk_file(path)` is a host file the user manages directly.
    // `Qemu::disk(&image)` references an `Image` or a build step
    // declared in the same script, found among the artifacts.
    if let Some(disk) = &def.disk {
        resolved.disk = Some(resolve_disk_source(disk, &request.artifacts)?);
    }

    // Auto-wire the ESP directory for UEFI boot.
    //
    // A test's ESP copy wins over everything. Otherwise, if the user
    // has NOT set an explicit `Qemu::esp_dir` / `Qemu::esp_image` and
    // the project declares exactly one `Esp`, point QEMU at
    // the ESP directory the build assembled (or, under `--dry-run` /
    // `--no-build`, at the path it would have). More than one declared
    // ESP is an error: the user must pick explicitly because we can't
    // guess. Zero declarations falls through, and QEMU has no ESP drive.
    let uefi = resolved.boot_mode == BootMode::Uefi;
    if let Some(dir) = request.boot_esp.clone().filter(|_| uefi) {
        resolved.esp = Some(EspSource::Dir(dir));
    } else if uefi && resolved.esp.is_none() {
        let esps: Vec<_> = request
            .artifacts
            .iter()
            .filter(|a| a.kind == ArtifactKind::Esp)
            .collect();
        match esps.as_slice() {
            [] => {}
            [esp] => resolved.esp = esp.path.clone().map(EspSource::Dir),
            many => {
                return Err(Error::Compile(format!(
                    "profile '{}' boots with BootMode::Uefi and the project declares {} ESPs; \
                     quon can't guess which one to mount. Set Qemu::esp_dir(..) explicitly, \
                     or remove the extra Esp declarations.",
                    context.profile,
                    many.len()
                )));
            }
        }
    }

    let ovmf = if uefi {
        let ctx = OvmfResolveCtx::from_env(&context.build_dir, &context.profile, &resolved.binary);
        Some(resolve_ovmf(&def, &ctx)?)
    } else {
        None
    };

    // `--gdb` goes first among the pass-through arguments. This keeps
    // `build_qemu_command` pure, and it places the gdb flags *after*
    // the runner's `extra_args`, so a user who wants another gdb port
    // (`-gdb tcp::9999`) can set it in their config without being
    // clobbered. `test_extra_args` follow, then the command line's.
    let mut extra: Vec<OsString> = Vec::new();
    if request.gdb {
        extra.push("-s".into());
        extra.push("-S".into());
    }
    if context.test {
        extra.extend(def.test_extra_args.iter().map(OsString::from));
    }
    extra.extend(request.extra_args.iter().map(OsString::from));

    let command = build_qemu_command(
        &resolved,
        &request.binary,
        resolved.boot_mode,
        ovmf.as_ref(),
        &extra,
        // No request field turns the `isa-debug-exit` device on:
        // projects such as Hadron add it in `extra_args`, and a second
        // device on the same port would clash.
        false,
        // Without a build the ESP and disk paths may not exist yet,
        // and QEMU reports a missing one better than we can.
        !request.built,
    )?;

    let mut notes = Vec::new();
    if request.gdb {
        // QEMU halts waiting for the debugger and prints nothing until
        // one connects, so say where to connect.
        notes.push("QEMU halted for GDB on :1234; connect with `target remote :1234`".into());
    }
    Ok(PrepareResponse {
        program: command.binary,
        args: command
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        env: BTreeMap::new(),
        success_exit_code: def.success_exit_code,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quon_runner::Artifact;
    use serde_json::json;

    fn context(settings: serde_json::Value) -> Context {
        Context {
            runner: "vm".into(),
            profile: "dev".into(),
            target: Some("x86_64-unknown-none".into()),
            build_dir: "/p/build".into(),
            settings: serde_json::from_value(settings).unwrap(),
            ..Context::default()
        }
    }

    fn request(context: Context) -> PrepareRequest {
        PrepareRequest {
            context,
            binary: "/p/build/kernel".into(),
            built: false,
            ..PrepareRequest::default()
        }
    }

    #[test]
    fn validate_reports_how_tests_boot() {
        let cases = [
            ("direct", TestBinary::AsBinary),
            ("uefi", TestBinary::EspCopy),
        ];
        for (mode, expected) in cases {
            let mut c = context(json!({"test_timeout": 5}));
            c.overrides.insert("boot_mode".into(), json!(mode));
            let v = QemuRunner.validate(&c).unwrap();
            assert_eq!(v.test_binary, expected, "{mode}");
            assert_eq!(v.test_timeout, Some(5));
        }
        let v = QemuRunner
            .validate(&context(json!({"boot_mode": "bios"})))
            .unwrap();
        assert!(matches!(v.test_binary, TestBinary::Unsupported { .. }));
    }

    #[test]
    fn validate_rejects_bad_settings_and_host_profiles() {
        let err = QemuRunner
            .validate(&context(json!({"memory_mb": "lots"})))
            .unwrap_err();
        assert!(
            err.0
                .starts_with("configuration error: runner 'vm': invalid QEMU settings")
        );

        let mut c = context(json!({}));
        c.overrides.insert("bogus".into(), json!(1));
        let err = QemuRunner.validate(&c).unwrap_err();
        assert!(err.0.contains("invalid QEMU override"), "{err}");

        let mut c = context(json!({}));
        c.target = None;
        let err = QemuRunner.validate(&c).unwrap_err();
        assert!(err.0.contains("booting under QEMU needs a target"), "{err}");
    }

    #[test]
    fn prepare_assembles_the_direct_boot_command() {
        let mut req = request(context(
            json!({"binary": "qemu-system-x86_64", "memory_mb": 128}),
        ));
        req.context.overrides.insert("cores".into(), json!(2));
        req.extra_args = vec!["-no-reboot".into()];
        req.gdb = true;
        let resp = QemuRunner.prepare(&req).unwrap();
        assert_eq!(resp.program, "qemu-system-x86_64");
        assert_eq!(
            resp.args,
            [
                "-machine",
                "q35",
                "-m",
                "128M",
                "-smp",
                "2",
                "-display",
                "none",
                "-serial",
                "stdio",
                "-kernel",
                "/p/build/kernel",
                "-s",
                "-S",
                "-no-reboot",
            ]
        );
        assert_eq!(resp.notes.len(), 1);
    }

    #[test]
    fn prepare_adds_test_extra_args_only_under_test() {
        let mut req = request(context(json!({"test_extra_args": ["-t"]})));
        req.extra_args = vec!["-cli".into()];
        let plain = QemuRunner.prepare(&req).unwrap();
        assert_eq!(plain.args.last().unwrap(), "-cli");
        assert!(!plain.args.iter().any(|a| a == "-t"));
        req.context.test = true;
        let test = QemuRunner.prepare(&req).unwrap();
        assert_eq!(test.args[test.args.len() - 2..], ["-t", "-cli"]);
    }

    #[test]
    fn prepare_wires_the_one_esp_or_the_test_copy_for_uefi() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("CODE.fd");
        let vars = tmp.path().join("VARS.fd");
        std::fs::write(&code, b"c").unwrap();
        std::fs::write(&vars, b"v").unwrap();
        let mut req = request(context(json!({
            "boot_mode": "uefi",
            "ovmf_code": code,
            "ovmf_vars": vars,
            "success_exit_code": 33,
        })));
        req.context.build_dir = tmp.path().to_path_buf();
        req.artifacts = vec![Artifact {
            name: "boot".into(),
            kind: ArtifactKind::Esp,
            path: Some("/p/build/esp/boot".into()),
        }];
        let resp = QemuRunner.prepare(&req).unwrap();
        assert!(
            resp.args
                .iter()
                .any(|a| a == "format=raw,file=fat:rw:/p/build/esp/boot"),
            "{:?}",
            resp.args
        );
        assert_eq!(resp.success_exit_code, Some(33));

        req.boot_esp = Some("/p/build/esp/boot.tests/t1".into());
        let resp = QemuRunner.prepare(&req).unwrap();
        assert!(
            resp.args
                .iter()
                .any(|a| a == "format=raw,file=fat:rw:/p/build/esp/boot.tests/t1"),
            "{:?}",
            resp.args
        );

        req.boot_esp = None;
        req.artifacts.push(Artifact {
            name: "other".into(),
            kind: ArtifactKind::Esp,
            path: Some("/p/build/esp/other".into()),
        });
        let err = QemuRunner.prepare(&req).unwrap_err();
        assert!(err.0.contains("declares 2 ESPs"), "{err}");
    }
}
