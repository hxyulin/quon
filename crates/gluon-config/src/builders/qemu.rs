//! Chainable builder for the singleton `QemuDef`.

use crate::builders::BuilderState;
use crate::config::Config;
use crate::error::ConfigError;
use gluon_model::{BootMode, DiskSource, DisplayMode, EspSource, SerialMode};
use std::path::PathBuf;

/// Chainable builder returned by [`Config::qemu`].
///
/// There is no arena for the singleton `QemuDef`; the handle type
/// parameter is `()`. All chained methods access `cfg.raw.qemu`
/// directly via [`modify`].
pub struct QemuBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    /// Singleton: no arena handle to capture — `state` carries `()`.
    pub(crate) state: BuilderState<()>,
}

impl<'cfg> QemuBuilder<'cfg> {
    /// Apply `f` to the `QemuDef` if this builder is `Valid`.
    ///
    /// Short-circuits silently when in `Duplicate` state so the first
    /// call's values are never overwritten by a second call. The
    /// `as_mut().unwrap()` is safe because `Config::qemu` populates
    /// `Some(QemuDef)` before returning a `Valid` builder; `Duplicate`
    /// short-circuits and never touches the field.
    fn modify<F: FnOnce(&mut gluon_model::QemuDef)>(&mut self, f: F) {
        if !self.state.is_duplicate() {
            f(self
                .cfg
                .raw
                .qemu
                .as_mut()
                .expect("Valid qemu builder always has Some(QemuDef)"));
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
                // would falsely accuse the second `qemu()` caller of the parsing error.
                if !self.state.is_duplicate() {
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
    pub fn esp_dir(self, path: impl Into<PathBuf>) -> Self {
        if !self.state.is_duplicate() {
            // `as_ref().unwrap()`: Valid builders always carry Some(QemuDef).
            // We read `esp` first (immutable borrow), then drop that borrow
            // before either pushing a diagnostic on `self.cfg.diagnostics`
            // or mutating `self.cfg.raw.qemu`. Doing it in one expression
            // would conflict because both targets sit under `self.cfg`.
            let already_set = self
                .cfg
                .raw
                .qemu
                .as_ref()
                .expect("Valid qemu builder always has Some(QemuDef)")
                .esp
                .is_some();
            if already_set {
                // Mirror engine wording verbatim.
                self.cfg.diagnostics.push(ConfigError::new(
                    "qemu: esp_dir / esp_image may only be called once per profile".to_string(),
                ));
            } else {
                let p = path.into();
                self.cfg
                    .raw
                    .qemu
                    .as_mut()
                    .expect("Valid qemu builder always has Some(QemuDef)")
                    .esp = Some(EspSource::Dir(p));
            }
        }
        self
    }

    /// Use a pre-built ESP image file.
    ///
    /// Mutually exclusive with [`esp_dir`]: the first call wins and the
    /// second pushes a diagnostic.
    pub fn esp_image(self, path: impl Into<PathBuf>) -> Self {
        if !self.state.is_duplicate() {
            let already_set = self
                .cfg
                .raw
                .qemu
                .as_ref()
                .expect("Valid qemu builder always has Some(QemuDef)")
                .esp
                .is_some();
            if already_set {
                // Mirror engine wording verbatim.
                self.cfg.diagnostics.push(ConfigError::new(
                    "qemu: esp_dir / esp_image may only be called once per profile".to_string(),
                ));
            } else {
                let p = path.into();
                self.cfg
                    .raw
                    .qemu
                    .as_mut()
                    .expect("Valid qemu builder always has Some(QemuDef)")
                    .esp = Some(EspSource::Image(p));
            }
        }
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
    use gluon_model::{BootMode, EspSource, SerialMode};
    use std::path::PathBuf;

    #[test]
    fn qemu_typed_setters_round_trip() {
        let mut cfg = Config::new();
        cfg.qemu("qemu-system-x86_64")
            .machine("q35")
            .memory(512)
            .cores(4)
            .serial_stdio()
            .boot_mode("uefi")
            .ovmf_code("/opt/ovmf/CODE.fd")
            .ovmf_vars("/opt/ovmf/VARS.fd")
            .esp_dir("./build/esp");

        let q = cfg.raw.qemu.as_ref().expect("qemu populated");
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
    fn duplicate_qemu_short_circuits_and_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.qemu("a").machine("q35");
        cfg.qemu("b").machine("pc");
        let q = cfg.raw.qemu.as_ref().expect("qemu populated");
        assert_eq!(
            q.binary.as_deref(),
            Some("a"),
            "first call's binary preserved"
        );
        assert_eq!(
            q.machine.as_deref(),
            Some("q35"),
            "second call's machine must NOT clobber the first"
        );
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(
            cfg.diagnostics[0]
                .message
                .contains("defined more than once")
        );
    }

    #[test]
    fn qemu_boot_mode_unknown_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.qemu("a").boot_mode("legacy");
        assert!(cfg.diagnostics.iter().any(|d| d.message.contains("legacy")));
        assert_eq!(
            cfg.raw.qemu.as_ref().expect("qemu populated").boot_mode,
            None,
            "unknown boot_mode must NOT be silently ignored AND must not be set"
        );
    }

    #[test]
    fn qemu_esp_mutually_exclusive() {
        let mut cfg = Config::new();
        cfg.qemu("a").esp_dir("a").esp_image("b.img");
        // First call wins.
        match &cfg.raw.qemu.as_ref().expect("qemu populated").esp {
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
        cfg.qemu("q")
            .extra_args(&["-display", "none"])
            .extra_args(&["-no-reboot"]);
        assert_eq!(
            cfg.raw.qemu.as_ref().expect("qemu populated").extra_args,
            vec![
                "-display".to_string(),
                "none".to_string(),
                "-no-reboot".to_string()
            ],
        );
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn success_exit_code_round_trips() {
        let mut cfg = Config::new();
        cfg.qemu("q").success_exit_code(33);
        let q = cfg.raw.qemu.as_ref().expect("qemu populated");
        assert_eq!(q.success_exit_code, Some(33));
        let json = serde_json::to_string(&cfg.raw).unwrap();
        assert!(json.contains("\"success_exit_code\":33"), "got: {json}");
    }

    #[test]
    fn success_exit_code_unset_by_default() {
        let mut cfg = Config::new();
        cfg.qemu("q");
        let q = cfg.raw.qemu.as_ref().expect("qemu populated");
        assert_eq!(q.success_exit_code, None);
    }

    #[test]
    fn unset_qemu_is_none() {
        // Singleton-set is now "field is Some(...)". A fresh config that
        // never called `qemu(...)` round-trips with `qemu` omitted.
        let cfg = Config::new();
        assert!(cfg.raw.qemu.is_none());
        let json = serde_json::to_string(&cfg.raw).unwrap();
        assert!(
            !json.contains("\"qemu\""),
            "absent qemu must not be serialized, got: {json}"
        );
    }
}
