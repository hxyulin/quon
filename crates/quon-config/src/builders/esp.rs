//! Chainable builder for `EspDef` entries.

use crate::builders::BuilderState;
use crate::config::Config;
use crate::error::ConfigError;
use quon_model::{Handle, RawEspDef, RawEspEntry, RawEspSource};

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
        if let BuilderState::Valid { state: handle, .. } = &self.state
            && let Some(e) = self.cfg.raw.esps.get_mut(*handle)
        {
            f(e);
        }
    }

    /// Append an ESP entry: copy `source_crate`'s primary output to
    /// `dest_path` within the ESP directory.
    ///
    /// `dest_path` must be relative to the ESP root (no leading `/` or `\`).
    /// An absolute `dest_path` is rejected with an engine-parity diagnostic
    /// and the entry is **not** inserted — mirroring the engine check at
    /// `register_esp` (pipeline.rs line 961).
    pub fn add(self, source_crate: &str, dest_path: &str) -> Self {
        self.push("add", RawEspSource::Crate(source_crate.into()), dest_path)
    }

    /// Append an ESP entry: copy the host file `path`, relative to the
    /// project root, to `dest_path` within the ESP directory. Use it
    /// for files Quon does not build, such as a bootloader's
    /// configuration file. `dest_path` follows the rules of [`Self::add`].
    pub fn add_file(self, path: &str, dest_path: &str) -> Self {
        self.push("add_file", RawEspSource::File(path.into()), dest_path)
    }

    /// Append an ESP entry: copy the pinned download `name` (declared
    /// with `cfg.download(...)` or `cfg.limine(...)`) to `dest_path`.
    /// `dest_path` follows the rules of [`Self::add`].
    pub fn add_download(self, name: &str, dest_path: &str) -> Self {
        self.push(
            "add_download",
            RawEspSource::Download(name.into()),
            dest_path,
        )
    }

    fn push(mut self, method: &str, source: RawEspSource, dest_path: &str) -> Self {
        if self.state.is_duplicate() {
            return self;
        }
        // Mirror engine: reject leading '/' or '\'.
        if dest_path.starts_with('/') || dest_path.starts_with('\\') {
            let esp_name = self.state.name().to_string();
            self.cfg.diagnostics.push(ConfigError::new(format!(
                "esp(\"{esp_name}\").{method}: destination path '{dest_path}' must be relative \
                 to the ESP root (no leading '/')"
            )));
            return self;
        }
        let entry = RawEspEntry {
            source,
            dest_path: dest_path.into(),
        };
        self.modify(|e| e.entries.push(entry));
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;
    use quon_model::RawEspSource;

    #[test]
    fn esp_add_appends_entries() {
        let mut cfg = Config::new();
        cfg.esp("default")
            .add("bootloader", "EFI/BOOT/BOOTX64.EFI")
            .add("kernel", "kernel.elf");
        let h = cfg.raw.esps.lookup("default").unwrap();
        let e = cfg.raw.esps.get(h).unwrap();
        assert_eq!(e.entries.len(), 2);
        assert_eq!(
            e.entries[0].source,
            RawEspSource::Crate("bootloader".into())
        );
        assert_eq!(e.entries[0].dest_path, "EFI/BOOT/BOOTX64.EFI");
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn esp_add_file_appends_host_file_entry() {
        let mut cfg = Config::new();
        cfg.esp("default")
            .add_file("boot/limine.conf", "limine.conf")
            .add_file("x", "/abs");
        let h = cfg.raw.esps.lookup("default").unwrap();
        let e = cfg.raw.esps.get(h).unwrap();
        assert_eq!(e.entries.len(), 1);
        assert_eq!(
            e.entries[0].source,
            RawEspSource::File("boot/limine.conf".into())
        );
        assert_eq!(e.entries[0].dest_path, "limine.conf");
        assert!(cfg.diagnostics[0].message.contains("add_file"));
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
        assert_eq!(e.entries[0].source, RawEspSource::Crate("bl".into()));
    }
}
