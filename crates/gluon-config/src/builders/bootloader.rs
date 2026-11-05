//! Chainable builder for the singleton `BootloaderDef`.

use crate::builders::BuilderState;
use crate::config::Config;

/// Chainable builder returned by [`Config::bootloader`].
///
/// There is no arena for the singleton `BootloaderDef`; the handle type
/// parameter is `()`. All chained methods access
/// `cfg.raw.bootloader` directly via [`modify`].
pub struct BootloaderBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    /// Singleton: no arena handle to capture — `state` carries `()`.
    pub(crate) state: BuilderState<()>,
}

impl<'cfg> BootloaderBuilder<'cfg> {
    /// Apply `f` to the `RawBootloaderDef` if this builder is `Valid`.
    ///
    /// `cfg.raw.bootloader` is `Option<RawBootloaderDef>`. The `Valid`
    /// arm of `Config::bootloader` always populates `Some(...)` before
    /// returning the builder, so the `as_mut().unwrap()` here is the
    /// safe shape for the singleton: `Duplicate` short-circuits and
    /// never touches the field, `Valid` is guaranteed to have a payload.
    fn modify<F: FnOnce(&mut gluon_model::RawBootloaderDef)>(&mut self, f: F) {
        if !self.state.is_duplicate() {
            f(self
                .cfg
                .raw
                .bootloader
                .as_mut()
                .expect("Valid bootloader builder always has Some(RawBootloaderDef)"));
        }
    }

    /// Set the boot protocol sub-variant (e.g. `"gop"` for UEFI with GOP).
    pub fn protocol(mut self, proto: &str) -> Self {
        let p = proto.to_string();
        self.modify(|bl| bl.protocol = Some(p));
        self
    }

    /// Set the name of the crate that serves as the bootloader entry point.
    /// The intern pass resolves this name into a [`Handle<CrateDef>`] on
    /// the resolved [`gluon_model::BootloaderDef`].
    pub fn entry_crate(mut self, name: &str) -> Self {
        let n = name.to_string();
        self.modify(|bl| bl.entry_crate = Some(n));
        self
    }

    /// Set the bootloader configuration file path (e.g. `"boot.cfg"`).
    pub fn config_file(mut self, file: &str) -> Self {
        let f = file.to_string();
        self.modify(|bl| bl.config_file = Some(f));
        self
    }

    /// Insert a protocol-specific key-value pair into `extras`. Used as an
    /// escape hatch for metadata that does not yet have a typed field.
    pub fn extra(mut self, key: &str, value: &str) -> Self {
        let k = key.to_string();
        let v = value.to_string();
        self.modify(|bl| {
            bl.extras.insert(k, v);
        });
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;
    use gluon_model::BootloaderKind;

    #[test]
    fn bootloader_chain_sets_typed_fields() {
        let mut cfg = Config::new();
        cfg.bootloader("uefi")
            .entry_crate("loader")
            .config_file("boot.cfg");
        let bl = cfg.raw.bootloader.as_ref().expect("bootloader populated");
        assert_eq!(bl.kind, BootloaderKind::Uefi);
        assert_eq!(bl.entry_crate.as_deref(), Some("loader"));
        assert_eq!(bl.config_file.as_deref(), Some("boot.cfg"));
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn duplicate_bootloader_short_circuits_and_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.bootloader("a").entry_crate("loader_a");
        cfg.bootloader("b").entry_crate("loader_b");
        let bl = cfg.raw.bootloader.as_ref().expect("bootloader populated");
        // Unknown kinds round-trip through BootloaderKind::Custom.
        assert_eq!(
            bl.kind,
            BootloaderKind::Custom("a".into()),
            "first kind preserved",
        );
        assert_eq!(bl.entry_crate.as_deref(), Some("loader_a"));
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(
            cfg.diagnostics[0]
                .message
                .contains("defined more than once")
        );
    }
}
