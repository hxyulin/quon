//! Chainable builder for `EspDef` entries.

use crate::builders::BuilderState;
use crate::config::Config;
use crate::error::ConfigError;
use gluon_model::{Handle, RawEspDef, RawEspEntry};

/// Chainable builder returned by [`Config::esp`].
pub struct EspBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<Handle<RawEspDef>>,
}

impl<'cfg> EspBuilder<'cfg> {
    /// Apply `f` to the `RawEspDef` if this builder is `Valid`.
    /// Uses the captured handle directly — no per-call name lookup or string
    /// allocation.
    fn modify<F: FnOnce(&mut RawEspDef)>(&mut self, f: F) {
        if let BuilderState::Valid { state: handle, .. } = &self.state {
            if let Some(e) = self.cfg.raw.esps.get_mut(*handle) {
                f(e);
            }
        }
    }

    /// Append an ESP entry: copy `source_crate`'s primary output to
    /// `dest_path` within the ESP directory.
    ///
    /// `dest_path` must be relative to the ESP root (no leading `/` or `\`).
    /// An absolute `dest_path` is rejected with an engine-parity diagnostic
    /// and the entry is **not** inserted — mirroring the engine check at
    /// `register_esp` (pipeline.rs line 961).
    pub fn add(mut self, source_crate: &str, dest_path: &str) -> Self {
        if self.state.is_duplicate() {
            return self;
        }
        // Mirror engine: reject leading '/' or '\'.
        if dest_path.starts_with('/') || dest_path.starts_with('\\') {
            let esp_name = self.state.name().to_string();
            self.cfg.diagnostics.push(ConfigError::new(format!(
                "esp(\"{esp_name}\").add: destination path '{dest_path}' must be relative \
                 to the ESP root (no leading '/')"
            )));
            return self;
        }
        let entry = RawEspEntry {
            source_crate: source_crate.into(),
            dest_path: dest_path.into(),
        };
        self.modify(|e| e.entries.push(entry));
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    #[test]
    fn esp_add_appends_entries() {
        let mut cfg = Config::new();
        cfg.esp("default")
            .add("bootloader", "EFI/BOOT/BOOTX64.EFI")
            .add("kernel", "kernel.elf");
        let h = cfg.raw.esps.lookup("default").unwrap();
        let e = cfg.raw.esps.get(h).unwrap();
        assert_eq!(e.entries.len(), 2);
        assert_eq!(e.entries[0].source_crate, "bootloader");
        assert_eq!(e.entries[0].dest_path, "EFI/BOOT/BOOTX64.EFI");
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn esp_add_rejects_absolute_dest_path() {
        let mut cfg = Config::new();
        cfg.esp("default").add("bl", "/EFI/BOOT/BOOTX64.EFI");
        assert!(
            cfg.diagnostics
                .iter()
                .any(|d| d.message.contains("relative to the ESP root"))
        );
        let h = cfg.raw.esps.lookup("default").unwrap();
        let e = cfg.raw.esps.get(h).unwrap();
        assert!(e.entries.is_empty(), "rejected entry must NOT be inserted");
    }

    #[test]
    fn duplicate_esp_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.esp("default").add("bl", "EFI/BOOT/BOOTX64.EFI");
        cfg.esp("default").add("bl2", "EFI/BOOT/BOOTIA32.EFI");
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(cfg.diagnostics[0].message.contains("default"));
        let h = cfg.raw.esps.lookup("default").unwrap();
        let e = cfg.raw.esps.get(h).unwrap();
        assert_eq!(
            e.entries.len(),
            1,
            "first esp's entry preserved; second's add no-op"
        );
        assert_eq!(e.entries[0].source_crate, "bl");
    }
}
