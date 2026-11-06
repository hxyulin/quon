//! Runners: how a profile's run binary is executed.
//!
//! A profile names a runner (`cfg.runner(name)`, `profile.runner(name)`)
//! and every place that starts a built binary goes through it:
//! `gluon run`, the integration tests and the boot tests. Host tests
//! are libtest executables and always run on the host (see
//! [`super::host`]).
//!
//! Each runner turns a binary into an [`Invocation`]:
//!
//! - a QEMU runner boots it, with the boot mode, ESP, OVMF and disk
//!   handling in `entry` and [`super::qemu_cmd`];
//! - a command runner runs its argv with `{binary}` replaced by the
//!   binary's path.

use crate::error::{Error, Result};
use gluon_model::{BuildModel, CommandRunnerDef, QemuDef, ResolvedConfig, RunnerKind};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

/// A process to start for a run or a test, plus how to judge it.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// The program: the QEMU binary, or a command runner's first word.
    pub binary: String,
    pub args: Vec<OsString>,
    pub timeout: Option<Duration>,
    /// The runner's name, for messages.
    pub runner: String,
    /// The exit status that counts as success. `None` means 0.
    pub success_exit_code: Option<i32>,
}

/// The runner of the resolved profile.
#[derive(Debug, Clone, Copy)]
pub enum Runner<'m> {
    Qemu {
        name: &'m str,
        def: &'m QemuDef,
    },
    Command {
        name: &'m str,
        def: &'m CommandRunnerDef,
    },
}

impl<'m> Runner<'m> {
    pub fn name(&self) -> &'m str {
        match self {
            Runner::Qemu { name, .. } | Runner::Command { name, .. } => name,
        }
    }

    /// The QEMU configuration, for a QEMU runner.
    pub fn qemu(&self) -> Option<&'m QemuDef> {
        match self {
            Runner::Qemu { def, .. } => Some(def),
            Runner::Command { .. } => None,
        }
    }

    /// Fail when a QEMU-only `option` was asked of another runner.
    pub(crate) fn require_qemu(&self, asked: bool, option: &'static str) -> Result<()> {
        match self {
            Runner::Command { name, .. } if asked => Err(Error::QemuOnlyOption {
                runner: (*name).to_string(),
                kind: "command",
                option,
            }),
            _ => Ok(()),
        }
    }
}

/// The runner the resolved profile names, directly or through
/// `inherits`.
pub fn profile_runner<'m>(model: &'m BuildModel, resolved: &ResolvedConfig) -> Result<Runner<'m>> {
    let profile = &resolved.profile;
    let def = profile
        .runner
        .and_then(|h| model.runners.get(h))
        .ok_or_else(|| Error::NoRunner {
            profile: profile.name.clone(),
        })?;
    Ok(match &def.kind {
        RunnerKind::Qemu(q) => Runner::Qemu {
            name: &def.name,
            def: q,
        },
        RunnerKind::Command(c) => Runner::Command {
            name: &def.name,
            def: c,
        },
    })
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
        timeout,
        runner: name.to_string(),
        success_exit_code: def.success_exit_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
