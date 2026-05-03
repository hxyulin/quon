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
    /// Apply `f` to the `BootloaderDef` if this builder is `Valid`.
    fn modify<F: FnOnce(&mut gluon_model::RawBootloaderDef)>(&mut self, f: F) {
        if !self.state.is_duplicate() {
            f(&mut self.cfg.raw.bootloader);
        }
    }

    /// Set the boot protocol sub-variant (e.g. `"gop"` for UEFI with GOP).
    pub fn protocol(mut self, proto: &str) -> Self {
        let p = proto.to_string();
        self.modify(|bl| bl.protocol = Some(p));
        self
    }

    /// Set the name of the crate that serves as the bootloader entry point.
    /// The `entry_crate_handle` is populated later by the intern pass.
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

    #[test]
    fn bootloader_chain_sets_typed_fields() {
        let mut cfg = Config::new();
        cfg.bootloader("uefi").entry_crate("loader").config_file("boot.cfg");
        let bl = &cfg.raw.bootloader;
        assert_eq!(bl.kind, "uefi");
        assert_eq!(bl.entry_crate.as_deref(), Some("loader"));
        assert_eq!(bl.config_file.as_deref(), Some("boot.cfg"));
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn duplicate_bootloader_short_circuits_and_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.bootloader("a").entry_crate("loader_a");
        cfg.bootloader("b").entry_crate("loader_b");
        assert_eq!(cfg.raw.bootloader.kind, "a", "first kind preserved");
        assert_eq!(cfg.raw.bootloader.entry_crate.as_deref(), Some("loader_a"));
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(cfg.diagnostics[0].message.contains("defined more than once"));
    }

    #[test]
    fn bootloader_extras_are_recorded() {
        let mut cfg = Config::new();
        cfg.bootloader("custom").extra("flavor", "limine").extra("strict", "true");
        let bl = &cfg.raw.bootloader;
        assert_eq!(bl.extras.get("flavor").map(String::as_str), Some("limine"));
        assert_eq!(bl.extras.get("strict").map(String::as_str), Some("true"));
    }

    #[test]
    fn bootloader_was_set_does_not_serialize() {
        let mut cfg = Config::new();
        cfg.bootloader("uefi");
        let json = serde_json::to_string(&cfg.raw).unwrap();
        assert!(!json.contains("bootloader_was_set"),
            "bootloader_was_set must be #[serde(skip)] — not serialized");
    }
}
