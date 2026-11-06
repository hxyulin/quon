//! Builders returned by [`crate::Config::runner`].

use crate::builders::QemuBuilder;
use crate::config::Config;
use gluon_model::{CommandRunnerDef, Handle, PluginRunnerDef, QemuDef, RawRunnerDef, RunnerKind};

/// Chainable builder returned by [`Config::runner`]. Pick the kind with
/// [`qemu`](Self::qemu), [`command`](Self::command) or
/// [`plugin`](Self::plugin).
pub struct RunnerBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    /// `None` for a repeated name.
    pub(crate) handle: Option<Handle<RawRunnerDef>>,
}

impl<'cfg> RunnerBuilder<'cfg> {
    fn set_kind(&mut self, kind: RunnerKind) {
        if let Some(h) = self.handle
            && let Some(r) = self.cfg.raw.runners.get_mut(h)
        {
            r.kind = Some(kind);
        }
    }

    /// Boot the binary under QEMU, started as `program`.
    pub fn qemu(mut self, program: &str) -> QemuBuilder<'cfg> {
        self.set_kind(RunnerKind::Qemu(Box::new(QemuDef {
            binary: Some(program.into()),
            ..Default::default()
        })));
        QemuBuilder {
            cfg: self.cfg,
            handle: self.handle,
        }
    }

    /// Run `argv`, with each `{binary}` replaced by the binary's path.
    /// The binary passes when the command exits with status 0, or the
    /// one set with [`CommandRunnerBuilder::success_exit_code`].
    ///
    /// ```rust,ignore
    /// cfg.runner("board").command(&["probe-rs", "run", "--chip", "nRF52840_xxAA", "{binary}"]);
    /// ```
    pub fn command(mut self, argv: &[&str]) -> CommandRunnerBuilder<'cfg> {
        self.set_kind(RunnerKind::Command(CommandRunnerDef {
            argv: argv.iter().map(|a| (*a).to_string()).collect(),
            success_exit_code: None,
        }));
        CommandRunnerBuilder {
            cfg: self.cfg,
            handle: self.handle,
        }
    }

    /// Hand the binary to the runner plugin `plugin`: one built into
    /// Gluon, or `gluon-runner-<plugin>` on `$PATH`. Its settings are
    /// set with [`PluginRunnerBuilder::setting`] and passed to the
    /// plugin as JSON.
    ///
    /// ```rust,ignore
    /// cfg.runner("board").plugin("probe-rs").setting("chip", "nRF52840_xxAA");
    /// ```
    pub fn plugin(mut self, plugin: &str) -> PluginRunnerBuilder<'cfg> {
        self.set_kind(RunnerKind::Plugin(PluginRunnerDef {
            plugin: plugin.into(),
            settings: Default::default(),
        }));
        PluginRunnerBuilder {
            cfg: self.cfg,
            handle: self.handle,
        }
    }
}

/// Chainable builder returned by [`RunnerBuilder::plugin`].
pub struct PluginRunnerBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) handle: Option<Handle<RawRunnerDef>>,
}

impl PluginRunnerBuilder<'_> {
    /// Set the plugin setting `key`. A later call with the same key
    /// replaces the value. What the keys mean is up to the plugin.
    pub fn setting(self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        if let Some(h) = self.handle
            && let Some(RunnerKind::Plugin(p)) = self
                .cfg
                .raw
                .runners
                .get_mut(h)
                .and_then(|r| r.kind.as_mut())
        {
            p.settings.insert(key.into(), value.into());
        }
        self
    }
}

/// Chainable builder returned by [`RunnerBuilder::command`].
pub struct CommandRunnerBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) handle: Option<Handle<RawRunnerDef>>,
}

impl CommandRunnerBuilder<'_> {
    /// Treat exit status `code` as success, and every other status, 0
    /// included, as failure.
    pub fn success_exit_code(self, code: i32) -> Self {
        if let Some(h) = self.handle
            && let Some(RunnerKind::Command(c)) = self
                .cfg
                .raw
                .runners
                .get_mut(h)
                .and_then(|r| r.kind.as_mut())
        {
            c.success_exit_code = Some(code);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;
    use gluon_model::{CommandRunnerDef, RunnerKind};

    #[test]
    fn command_runner_records_argv_and_success_code() {
        let mut cfg = Config::new();
        cfg.runner("board")
            .command(&["probe-rs", "run", "{binary}"])
            .success_exit_code(3);
        let h = cfg.raw.runners.lookup("board").unwrap();
        match &cfg.raw.runners.get(h).unwrap().kind {
            Some(RunnerKind::Command(c)) => assert_eq!(
                c,
                &CommandRunnerDef {
                    argv: vec!["probe-rs".into(), "run".into(), "{binary}".into()],
                    success_exit_code: Some(3),
                }
            ),
            other => panic!("expected a command runner, got {other:?}"),
        }
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn plugin_runner_records_its_settings() {
        let mut cfg = Config::new();
        cfg.runner("board")
            .plugin("probe-rs")
            .setting("chip", "nRF52840_xxAA")
            .setting("speed", 4000)
            .setting("speed", 8000);
        let h = cfg.raw.runners.lookup("board").unwrap();
        match &cfg.raw.runners.get(h).unwrap().kind {
            Some(RunnerKind::Plugin(p)) => {
                assert_eq!(p.plugin, "probe-rs");
                assert_eq!(
                    serde_json::to_value(&p.settings).unwrap(),
                    serde_json::json!({"chip": "nRF52840_xxAA", "speed": 8000})
                );
            }
            other => panic!("expected a plugin runner, got {other:?}"),
        }
    }
}
