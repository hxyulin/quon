//! Runners: how a profile's run binary is executed.
//!
//! A profile names a runner (`q.run(..).runner(&r)`, `Profile::runner(&r)`)
//! and every place that starts a built binary goes through it:
//! `quon run`, the integration tests and the boot tests. Host tests
//! are libtest executables and always run on the host (see
//! [`super::host`]).
//!
//! Each runner turns a binary into an [`Invocation`]:
//!
//! - a plugin runner asks its plugin (see [`quon_runner`]). QEMU is
//!   one: `q.add(Qemu::new(program))` is a runner served by the
//!   `qemu` plugin, with the `QemuDef` as its settings. Quon finds a
//!   plugin among the ones the caller registered in [`Runners`], then
//!   as `quon-runner-<plugin>` on `$PATH`;
//! - a command runner runs its argv with `{binary}` replaced by the
//!   binary's path;
//! - a host profile with no runner named runs the binary directly.

use crate::error::{Error, Result};
use quon_model::{
    BuildModel, CommandRunnerDef, QemuDef, ResolvedConfig, ResolvedProfile, RunnerKind, TargetSpec,
};
use quon_runner::External;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The plugin that serves `Qemu::new(..)` runners.
pub const QEMU_PLUGIN: &str = "qemu";

/// A process to start for a run or a test, plus how to judge it.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// The program: the QEMU binary, or a command runner's first word.
    pub binary: String,
    pub args: Vec<OsString>,
    /// Variables set for the process on top of Quon's environment.
    pub env: BTreeMap<String, String>,
    pub timeout: Option<Duration>,
    /// The runner's name, for messages.
    pub runner: String,
    /// The exit status that counts as success. `None` means 0.
    pub success_exit_code: Option<i32>,
    /// Lines the runner wants printed before the process starts, such
    /// as where to attach a debugger.
    pub notes: Vec<String>,
}

/// The runner of the resolved profile.
#[derive(Debug, Clone)]
pub enum Runner<'m> {
    /// Served by a runner plugin.
    Plugin {
        name: &'m str,
        plugin: &'m str,
        settings: BTreeMap<String, serde_json::Value>,
        /// The profile's overrides of `settings`.
        overrides: BTreeMap<String, serde_json::Value>,
    },
    Command {
        name: &'m str,
        def: &'m CommandRunnerDef,
    },
    /// Run the binary itself: the default for a host profile.
    Host,
}

impl<'m> Runner<'m> {
    pub fn name(&self) -> &'m str {
        match self {
            Runner::Plugin { name, .. } | Runner::Command { name, .. } => name,
            Runner::Host => "host",
        }
    }

    /// Fail when an `option` only a plugin runner takes was asked of
    /// another runner.
    pub(crate) fn require_plugin(&self, asked: bool, option: &'static str) -> Result<()> {
        match self {
            Runner::Command { name, .. } if asked => Err(Error::QemuOnlyOption {
                runner: (*name).to_string(),
                kind: "command",
                option,
            }),
            Runner::Host if asked => Err(Error::QemuOnlyOption {
                runner: "host".to_string(),
                kind: "host",
                option,
            }),
            _ => Ok(()),
        }
    }
}

/// The runner the resolved profile names, directly or through
/// `inherits`. A host profile that names none runs its binaries
/// directly.
pub fn profile_runner<'m>(model: &'m BuildModel, resolved: &ResolvedConfig) -> Result<Runner<'m>> {
    let profile = &resolved.profile;
    let Some(def) = profile.runner.and_then(|h| model.runners.get(h)) else {
        return match profile.target {
            TargetSpec::Host => Ok(Runner::Host),
            TargetSpec::Cross(_) => Err(Error::NoRunner {
                profile: profile.name.clone(),
            }),
        };
    };
    Ok(match &def.kind {
        RunnerKind::Qemu(q) => Runner::Plugin {
            name: &def.name,
            plugin: QEMU_PLUGIN,
            settings: qemu_settings(q)?,
            overrides: qemu_profile_overrides(profile)?,
        },
        RunnerKind::Plugin(p) => Runner::Plugin {
            name: &def.name,
            plugin: &p.plugin,
            settings: p.settings.clone(),
            overrides: BTreeMap::new(),
        },
        RunnerKind::Command(c) => Runner::Command {
            name: &def.name,
            def: c,
        },
    })
}

/// A `QemuDef` as the QEMU plugin's settings: its JSON form, which
/// leaves out the fields `quon.rs` did not set.
fn qemu_settings(def: &QemuDef) -> Result<BTreeMap<String, serde_json::Value>> {
    let mut settings = to_map(def)?;
    // Where the runner was declared is for Quon's diagnostics, not a
    // setting.
    settings.remove("span");
    Ok(settings)
}

/// The profile's `qemu_*` values, as overrides of the QEMU plugin's
/// settings of the same names.
fn qemu_profile_overrides(
    profile: &ResolvedProfile,
) -> Result<BTreeMap<String, serde_json::Value>> {
    #[derive(serde::Serialize)]
    struct ProfileOverrides<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        memory_mb: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cores: Option<u32>,
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        extra_args: &'a [String],
        #[serde(skip_serializing_if = "Option::is_none")]
        boot_mode: Option<quon_model::BootMode>,
    }
    to_map(&ProfileOverrides {
        memory_mb: profile.qemu_memory,
        cores: profile.qemu_cores,
        extra_args: &profile.qemu_extra_args,
        boot_mode: profile.qemu_boot_mode,
    })
}

fn to_map(value: &impl serde::Serialize) -> Result<BTreeMap<String, serde_json::Value>> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::Object(map)) => Ok(map.into_iter().collect()),
        Ok(other) => Err(Error::Config(format!(
            "internal: runner settings serialize to {other}, not an object"
        ))),
        Err(e) => Err(Error::Config(format!(
            "internal: runner settings do not serialize: {e}"
        ))),
    }
}

/// The runner plugins built into the calling program, by plugin name.
/// A name not registered here is looked up as `quon-runner-<name>` on
/// `$PATH`. The `quon` binary registers the QEMU runner as
/// [`QEMU_PLUGIN`].
#[derive(Clone, Default)]
pub struct Runners {
    builtin: BTreeMap<String, Arc<dyn quon_runner::Runner>>,
}

impl Runners {
    /// Serve the plugin `name` with `runner`, in process.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        runner: impl quon_runner::Runner + 'static,
    ) {
        self.builtin.insert(name.into(), Arc::new(runner));
    }

    /// The plugin `plugin`, which runner `runner` names.
    pub(crate) fn find(&self, runner: &str, plugin: &str) -> Result<Arc<dyn quon_runner::Runner>> {
        if let Some(r) = self.builtin.get(plugin) {
            return Ok(Arc::clone(r));
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        match External::find(plugin, &path) {
            Some(external) => Ok(Arc::new(external)),
            None => Err(Error::RunnerPluginNotFound {
                runner: runner.to_string(),
                plugin: plugin.to_string(),
            }),
        }
    }
}

impl std::fmt::Debug for Runners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.builtin.keys()).finish()
    }
}

/// The invocation of a command runner for `binary`: its argv with each
/// `{binary}` replaced, then `extra_args`.
pub(crate) fn command_invocation(
    name: &str,
    def: &CommandRunnerDef,
    binary: &Path,
    extra_args: &[OsString],
    timeout: Option<Duration>,
) -> Result<Invocation> {
    let mut words = def.argv.iter().map(|w| {
        // A path that is not UTF-8 goes in whole when the word is just
        // the placeholder; inside a longer word it is converted.
        if w == "{binary}" {
            binary.as_os_str().to_os_string()
        } else {
            OsString::from(w.replace("{binary}", &binary.to_string_lossy()))
        }
    });
    let program = words.next().ok_or_else(|| {
        Error::Config(format!(
            "runner '{name}': `.command(...)` needs at least the program to run"
        ))
    })?;
    let mut args: Vec<OsString> = words.collect();
    args.extend(extra_args.iter().cloned());
    Ok(Invocation {
        binary: program.to_string_lossy().into_owned(),
        args,
        env: BTreeMap::new(),
        timeout,
        runner: name.to_string(),
        success_exit_code: def.success_exit_code,
        notes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quon_model::{BootMode, Handle};
    use serde_json::json;

    #[test]
    fn command_invocation_fills_in_the_binary_and_appends_extra_args() {
        let def = CommandRunnerDef {
            argv: vec![
                "probe-rs".into(),
                "run".into(),
                "{binary}".into(),
                "--elf={binary}".into(),
            ],
            success_exit_code: Some(3),
        };
        let inv = command_invocation(
            "board",
            &def,
            Path::new("/b/kernel"),
            &["--speed".into(), "4000".into()],
            None,
        )
        .unwrap();
        assert_eq!(inv.binary, "probe-rs");
        assert_eq!(
            inv.args,
            ["run", "/b/kernel", "--elf=/b/kernel", "--speed", "4000"]
                .map(OsString::from)
                .to_vec()
        );
        assert_eq!(inv.runner, "board");
        assert_eq!(inv.success_exit_code, Some(3));
    }

    #[test]
    fn command_invocation_needs_a_program() {
        let def = CommandRunnerDef::default();
        let err = command_invocation("empty", &def, Path::new("/b/k"), &[], None).unwrap_err();
        assert!(
            err.to_string().contains("needs at least the program"),
            "{err}"
        );
    }

    #[test]
    fn qemu_def_and_profile_values_become_plugin_settings() {
        let def = QemuDef {
            binary: Some("qemu-system-x86_64".into()),
            memory_mb: Some(128),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(qemu_settings(&def).unwrap()).unwrap(),
            json!({"binary": "qemu-system-x86_64", "memory_mb": 128})
        );

        let profile = ResolvedProfile {
            name: "dev".into(),
            target: TargetSpec::Cross(Handle::new(0)),
            opt_level: quon_model::OptLevel::Zero,
            debug_info: false,
            lto: None,
            run_binary: None,
            runner: None,
            qemu_memory: Some(512),
            qemu_cores: None,
            qemu_extra_args: vec!["-d".into()],
            qemu_boot_mode: Some(BootMode::Uefi),
            test_timeout: None,
        };
        assert_eq!(
            serde_json::to_value(qemu_profile_overrides(&profile).unwrap()).unwrap(),
            json!({"memory_mb": 512, "extra_args": ["-d"], "boot_mode": "uefi"})
        );
    }

    #[test]
    fn an_unknown_plugin_names_the_executable_it_looked_for() {
        let err = Runners::default()
            .find("board", "no-such-plugin-xyz")
            .err()
            .unwrap();
        let msg = err.to_string();
        assert!(msg.contains("`quon-runner-no-such-plugin-xyz`"), "{msg}");
        assert!(msg.contains("runner 'board'"), "{msg}");
    }
}
