//! Runners: how `quon run` and `quon test` execute a binary.

use crate::artifact::{Artifact, ArtifactKind};
use crate::quon::{Declare, Quon};
use quon_model::{
    BootMode, CommandRunnerDef, DiskSource, DisplayMode, EspSource, PluginRunnerDef, QemuDef,
    RunnerKind, SerialMode,
};
use std::path::PathBuf;

/// A handle to a runner, from `q.add(Qemu::..)`, `q.add(Command::..)`
/// or `q.add(Plugin::..)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    pub(crate) name: String,
}

/// Boot the binary under QEMU.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// let qemu = q.add(
///     Qemu::new("qemu-system-x86_64")
///         .machine("q35")
///         .memory_mb(256)
///         .serial(Serial::Stdio),
/// );
/// # let _ = qemu;
/// ```
#[derive(Debug, Clone)]
#[must_use = "a runner does nothing until it is passed to `q.add`"]
pub struct Qemu {
    pub(crate) def: QemuDef,
    pub(crate) disk: Option<Artifact>,
}

impl Qemu {
    /// Start QEMU as `program`, such as `qemu-system-x86_64`.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            def: QemuDef {
                binary: Some(program.into()),
                ..QemuDef::default()
            },
            disk: None,
        }
    }

    /// `-machine <machine>`.
    pub fn machine(mut self, machine: impl Into<String>) -> Self {
        self.def.machine = Some(machine.into());
        self
    }

    /// `-m <mb>M`.
    pub fn memory_mb(mut self, mb: u32) -> Self {
        self.def.memory_mb = Some(mb);
        self
    }

    /// `-smp <n>`.
    pub fn cores(mut self, n: u32) -> Self {
        self.def.cores = Some(n);
        self
    }

    /// Where the first serial port goes.
    pub fn serial(mut self, serial: SerialMode) -> Self {
        self.def.serial = Some(serial);
        self
    }

    /// The display backend. QEMU runs headless by default.
    pub fn display(mut self, display: DisplayMode) -> Self {
        self.def.display = Some(display);
        self
    }

    /// How QEMU starts the binary. `quon run --uefi` and `--bios`
    /// override it.
    pub fn boot(mut self, mode: BootMode) -> Self {
        self.def.boot_mode = Some(mode);
        self
    }

    /// Append `args` after every argument Quon sets.
    pub fn args<S: AsRef<str>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        self.def
            .extra_args
            .extend(args.into_iter().map(|s| s.as_ref().to_owned()));
        self
    }

    /// Treat exit status `code` as success and every other status, 0
    /// included, as failure. QEMU's `isa-debug-exit` device exits with
    /// `(value << 1) | 1`, so a kernel that writes `0x10` needs 33.
    pub fn success_exit_code(mut self, code: i32) -> Self {
        self.def.success_exit_code = Some(code);
        self
    }

    /// The OVMF firmware for UEFI boot. Without it, the runner looks in
    /// `$OVMF_CODE` and `$OVMF_VARS`, then in known install paths.
    pub fn ovmf(mut self, code: impl Into<PathBuf>, vars: impl Into<PathBuf>) -> Self {
        self.def.ovmf_code = Some(code.into());
        self.def.ovmf_vars = Some(vars.into());
        self
    }

    /// Mount the host directory `path` as the EFI system partition.
    /// Without this or [`esp_image`](Self::esp_image), a UEFI boot
    /// mounts the project's [`Esp`](crate::Esp) when it has exactly one.
    pub fn esp_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.def.esp = Some(EspSource::Dir(path.into()));
        self
    }

    /// Use the disk image file at `path` as the EFI system partition.
    pub fn esp_image(mut self, path: impl Into<PathBuf>) -> Self {
        self.def.esp = Some(EspSource::Image(path.into()));
        self
    }

    /// Boot from `image`, declared with [`Image`](crate::Image).
    pub fn disk(mut self, image: &Artifact) -> Self {
        self.disk = Some(image.clone());
        self
    }

    /// Boot from the disk image file at `path`.
    pub fn disk_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.def.disk = Some(DiskSource::Image { path: path.into() });
        self
    }
}

impl Declare for Qemu {
    type Handle = Runner;
    fn declare(mut self, q: &mut Quon) -> Runner {
        if let Some(disk) = self.disk.take() {
            if disk.kind == ArtifactKind::Image {
                self.def.disk = Some(DiskSource::ImageRef { name: disk.name });
            } else {
                q.error(format!(
                    "Qemu::disk: '{}' is a {}, not an image",
                    disk.name,
                    disk.kind.describe()
                ));
            }
        }
        q.add_runner("qemu", RunnerKind::Qemu(Box::new(self.def)))
    }
}

/// Run a command, with each `{binary}` in it replaced by the binary's
/// path.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// q.add(Command::new(["probe-rs", "run", "--chip", "nRF52840_xxAA", "{binary}"]));
/// ```
#[derive(Debug, Clone)]
#[must_use = "a runner does nothing until it is passed to `q.add`"]
pub struct Command(CommandRunnerDef);

impl Command {
    pub fn new<S: AsRef<str>>(argv: impl IntoIterator<Item = S>) -> Self {
        Self(CommandRunnerDef {
            argv: argv.into_iter().map(|s| s.as_ref().to_owned()).collect(),
            success_exit_code: None,
        })
    }

    /// Treat exit status `code` as success and every other status, 0
    /// included, as failure.
    pub fn success_exit_code(mut self, code: i32) -> Self {
        self.0.success_exit_code = Some(code);
        self
    }
}

impl Declare for Command {
    type Handle = Runner;
    fn declare(self, q: &mut Quon) -> Runner {
        q.add_runner("command", RunnerKind::Command(self.0))
    }
}

/// Hand the binary to a runner plugin: one built into Quon, or
/// `quon-runner-<name>` on `$PATH`. The settings reach the plugin as
/// JSON; what they mean is up to the plugin.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// q.add(Plugin::new("probe-rs").setting("chip", "nRF52840_xxAA").setting("speed", 4000));
/// ```
#[derive(Debug, Clone)]
#[must_use = "a runner does nothing until it is passed to `q.add`"]
pub struct Plugin(PluginRunnerDef);

impl Plugin {
    pub fn new(name: impl Into<String>) -> Self {
        Self(PluginRunnerDef {
            plugin: name.into(),
            settings: Default::default(),
        })
    }

    /// Set `key`, replacing an earlier value. For arrays and objects,
    /// pass `quon_config::serde_json::json!(..)`.
    pub fn setting(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.0.settings.insert(key.into(), value.into());
        self
    }
}

impl Declare for Plugin {
    type Handle = Runner;
    fn declare(self, q: &mut Quon) -> Runner {
        let base = self.0.plugin.clone();
        q.add_runner(&base, RunnerKind::Plugin(self.0))
    }
}
