//! Chainable builder for a QEMU runner, returned by
//! [`crate::builders::RunnerBuilder::qemu`].

use crate::config::Config;
use crate::error::ConfigError;
use gluon_model::{
    BootMode, DiskSource, DisplayMode, EspSource, Handle, QemuDef, RawRunnerDef, RunnerKind,
    SerialMode,
};
use std::path::PathBuf;

/// Chainable builder for the [`QemuDef`] of one runner. Its methods do
/// nothing when the runner's name was a repeat, which
/// [`Config::runner`] reports as a diagnostic.
pub struct QemuBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    /// `None` for a repeated runner name.
    pub(crate) handle: Option<Handle<RawRunnerDef>>,
}

impl<'cfg> QemuBuilder<'cfg> {
    /// The runner's `QemuDef`. `RunnerBuilder::qemu` sets the kind
    /// before returning this builder, so it is `Some` whenever the
    /// handle is.
    fn qemu_mut(&mut self) -> Option<&mut QemuDef> {
        let h = self.handle?;
        match self.cfg.raw.runners.get_mut(h)?.kind.as_mut()? {
            RunnerKind::Qemu(q) => Some(q.as_mut()),
            RunnerKind::Command(_) => None,
        }
    }

    fn modify<F: FnOnce(&mut QemuDef)>(&mut self, f: F) {
        if let Some(q) = self.qemu_mut() {
            f(q);
        }
    }

    /// Set the ESP once; a second `esp_dir` or `esp_image` is reported.
    fn set_esp(&mut self, esp: EspSource) {
        match self.qemu_mut() {
            Some(q) if q.esp.is_none() => q.esp = Some(esp),
            Some(_) => self.cfg.diagnostics.push(ConfigError::new(
                "qemu: esp_dir / esp_image may only be called once per runner".to_string(),
            )),
            None => {}
        }
    }

    /// Set the QEMU machine type (`-machine <machine>`).
    pub fn machine(mut self, machine: &str) -> Self {
        let m = machine.to_string();
        self.modify(|q| q.machine = Some(m));
        self
    }

    /// Set RAM in MiB (`-m <memory>M`).
    pub fn memory(mut self, mb: u32) -> Self {
        self.modify(|q| q.memory_mb = Some(mb));
        self
    }

    /// Set the vCPU count (`-smp <cores>`).
    pub fn cores(mut self, n: u32) -> Self {
        self.modify(|q| q.cores = Some(n));
        self
    }

    /// Forward the first serial port to host stdio (`-serial stdio`).
    pub fn serial_stdio(mut self) -> Self {
        self.modify(|q| q.serial = Some(SerialMode::Stdio));
        self
    }

    /// Disable serial output (`-serial none`).
    pub fn serial_none(mut self) -> Self {
        self.modify(|q| q.serial = Some(SerialMode::None));
        self
    }

    /// Write serial output to a file (`-serial file:<path>`).
    pub fn serial_file(mut self, path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        self.modify(|q| q.serial = Some(SerialMode::File(p)));
        self
    }

    /// Set the display backend.
    pub fn display(mut self, mode: DisplayMode) -> Self {
        self.modify(|q| q.display = Some(mode));
        self
    }

    /// Append extra QEMU arguments. All arguments are appended after every
    /// gluon-managed flag.
    pub fn extra_args(mut self, args: &[&str]) -> Self {
        let v: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        self.modify(|q| q.extra_args.extend(v.iter().cloned()));
        self
    }

    /// Treat QEMU exit status `code` as success in `gluon run`.
    ///
    /// QEMU's `isa-debug-exit` device makes QEMU exit with
    /// `(value << 1) | 1`, where `value` is what the kernel wrote to the
    /// port. A kernel that writes `0x10` therefore needs
    /// `.success_exit_code(33)`. Once set, every other status, 0
    /// included, is reported as a failure.
    pub fn success_exit_code(mut self, code: i32) -> Self {
        self.modify(|q| q.success_exit_code = Some(code));
        self
    }

    /// Set the boot mode. Accepts `"direct"`, `"uefi"`, or `"bios"` (exact
    /// case, mirroring the engine). Unknown values push a diagnostic and leave
    /// the field unchanged.
    pub fn boot_mode(mut self, mode: &str) -> Self {
        let parsed = match mode {
            "direct" => Some(BootMode::Direct),
            "uefi" => Some(BootMode::Uefi),
            "bios" => Some(BootMode::Bios),
            other => {
                // Mirror engine wording verbatim so the user sees the same
                // message regardless of whether they use the DSL or the Rust API.
                // Diagnostic guard is manual here because `modify()` only gates the
                // model write; pushing a diagnostic when the builder is Duplicate
                // would falsely accuse the second `runner()` caller of the parsing error.
                if self.handle.is_some() {
                    self.cfg.diagnostics.push(ConfigError::new(format!(
                        "qemu.boot_mode: expected \"direct\", \"uefi\", or \"bios\", \
                         got \"{other}\""
                    )));
                }
                None
            }
        };
        if let Some(m) = parsed {
            self.modify(|q| q.boot_mode = Some(m));
        }
        self
    }

    /// Set an explicit OVMF CODE firmware path.
    pub fn ovmf_code(mut self, path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        self.modify(|q| q.ovmf_code = Some(p));
        self
    }

    /// Set an explicit OVMF VARS firmware path.
    pub fn ovmf_vars(mut self, path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        self.modify(|q| q.ovmf_vars = Some(p));
        self
    }

    /// Mount a host directory as the ESP via QEMU's VVFAT driver.
    ///
    /// Mutually exclusive with [`esp_image`]: the first call wins and the
    /// second pushes a diagnostic (mirroring the engine's behaviour).
    pub fn esp_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.set_esp(EspSource::Dir(path.into()));
        self
    }

    /// Use a pre-built ESP image file.
    ///
    /// Mutually exclusive with [`esp_dir`]: the first call wins and the
    /// second pushes a diagnostic.
    pub fn esp_image(mut self, path: impl Into<PathBuf>) -> Self {
        self.set_esp(EspSource::Image(path.into()));
        self
    }

    /// Attach a pre-built disk image as the primary boot disk.
    pub fn disk_image(mut self, path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        self.modify(|q| q.disk = Some(DiskSource::Image { path: p }));
        self
    }

    /// Reference an `image("name")` declared in the same config. Resolved
    /// to a path at qemu-spawn time.
    pub fn disk_image_ref(mut self, name: &str) -> Self {
        let n = name.to_string();
        self.modify(|q| q.disk = Some(DiskSource::ImageRef { name: n }));
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;
    use gluon_model::{BootMode, EspSource, QemuDef, RunnerKind, SerialMode};
    use std::path::PathBuf;

    fn qemu<'a>(cfg: &'a Config, name: &str) -> &'a QemuDef {
        let h = cfg.raw.runners.lookup(name).expect("runner declared");
        match cfg.raw.runners.get(h).and_then(|r| r.kind.as_ref()) {
            Some(RunnerKind::Qemu(q)) => q,
            other => panic!("expected a QEMU runner, got {other:?}"),
        }
    }

    #[test]
    fn qemu_typed_setters_round_trip() {
        let mut cfg = Config::new();
        cfg.runner("vm")
            .qemu("qemu-system-x86_64")
            .machine("q35")
            .memory(512)
            .cores(4)
            .serial_stdio()
            .boot_mode("uefi")
            .ovmf_code("/opt/ovmf/CODE.fd")
            .ovmf_vars("/opt/ovmf/VARS.fd")
            .esp_dir("./build/esp");

        let q = qemu(&cfg, "vm");
        assert_eq!(q.binary.as_deref(), Some("qemu-system-x86_64"));
        assert_eq!(q.machine.as_deref(), Some("q35"));
        assert_eq!(q.memory_mb, Some(512));
        assert_eq!(q.cores, Some(4));
        assert_eq!(q.serial, Some(SerialMode::Stdio));
        assert_eq!(q.boot_mode, Some(BootMode::Uefi));
        assert_eq!(
            q.ovmf_code.as_deref(),
            Some(std::path::Path::new("/opt/ovmf/CODE.fd"))
        );
        assert_eq!(q.esp, Some(EspSource::Dir(PathBuf::from("./build/esp"))));
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn duplicate_runner_short_circuits_and_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.runner("vm").qemu("a").machine("q35");
        cfg.runner("vm").qemu("b").machine("pc").boot_mode("legacy");
        let q = qemu(&cfg, "vm");
        assert_eq!(q.binary.as_deref(), Some("a"), "first call's binary kept");
        assert_eq!(
            q.machine.as_deref(),
            Some("q35"),
            "second call's machine must NOT clobber the first"
        );
        // Only the repeat is reported, not the repeat's bad boot mode.
        assert_eq!(cfg.diagnostics.len(), 1, "{:?}", cfg.diagnostics);
        assert!(
            cfg.diagnostics[0]
                .message
                .contains("defined more than once")
        );
    }

    #[test]
    fn qemu_boot_mode_unknown_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.runner("vm").qemu("a").boot_mode("legacy");
        assert!(cfg.diagnostics.iter().any(|d| d.message.contains("legacy")));
        assert_eq!(
            qemu(&cfg, "vm").boot_mode,
            None,
            "unknown boot_mode must NOT be silently ignored AND must not be set"
        );
    }

    #[test]
    fn qemu_esp_mutually_exclusive() {
        let mut cfg = Config::new();
        cfg.runner("vm").qemu("a").esp_dir("a").esp_image("b.img");
        // First call wins.
        match &qemu(&cfg, "vm").esp {
            Some(EspSource::Dir(p)) => assert_eq!(p, &PathBuf::from("a")),
            other => panic!("expected EspSource::Dir(\"a\"), got {other:?}"),
        }
        assert!(
            cfg.diagnostics
                .iter()
                .any(|d| d.message.contains("esp_dir") && d.message.contains("esp_image"))
        );
    }

    #[test]
    fn extra_args_appends_across_calls() {
        let mut cfg = Config::new();
        cfg.runner("vm")
            .qemu("q")
            .extra_args(&["-display", "none"])
            .extra_args(&["-no-reboot"]);
        assert_eq!(
            qemu(&cfg, "vm").extra_args,
            vec![
                "-display".to_string(),
                "none".to_string(),
                "-no-reboot".to_string()
            ],
        );
        assert!(cfg.diagnostics.is_empty());
    }
}
