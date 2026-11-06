//! Pinned downloads: `cfg.download(...)` and the `cfg.limine(...)`
//! helper.

use crate::config::Config;
use crate::error::ConfigError;
use quon_model::DownloadDef;

/// Where Limine publishes prebuilt binaries: one `v<version>-binary`
/// tag per release in the GitHub repository. Tags, unlike the
/// `v<major>.x-binary` branches, do not move, and the pinned hash
/// catches a tag that does. The Codeberg mirror is archived and stops
/// at 11.2.1.
const LIMINE_BINARY_URL: &str = "https://raw.githubusercontent.com/Limine-Bootloader/Limine";

impl Config {
    /// Declare a file fetched from `url` and checked against `sha256`
    /// (64 hex digits). ESP and image entries refer to it by `name`
    /// through `add_download` and `add_entry_download`.
    ///
    /// Quon fetches the file with `curl` the first time an entry
    /// needs it and caches it under `build/downloads/`. An empty
    /// `sha256` makes the build fail with the hash of the fetched
    /// file, which you can pin after checking where it came from.
    pub fn download(&mut self, name: &str, url: &str, sha256: &str) -> &mut Self {
        let sha256 = sha256.trim().to_ascii_lowercase();
        if !sha256.is_empty()
            && !(sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            self.diagnostics.push(ConfigError::new(format!(
                "download(\"{name}\"): '{sha256}' is not a SHA-256 (expected 64 hex digits)"
            )));
            return self;
        }
        let def = DownloadDef {
            name: name.into(),
            url: url.into(),
            sha256,
            span: None,
        };
        let (_, inserted) = self.raw.downloads.insert(name.into(), def);
        if !inserted {
            self.diagnostics.push(ConfigError::new(format!(
                "download(\"{name}\") is defined more than once"
            )));
        }
        self
    }

    /// Pin a Limine release. Returns a [`LimineBuilder`] whose
    /// [`file`](LimineBuilder::file) calls declare the release's
    /// binaries, each as a download named `limine/<file>`:
    ///
    /// ```rust,ignore
    /// cfg.limine("11.4.1").file("BOOTX64.EFI", "333f7a69...");
    /// cfg.esp("default").add_download("limine/BOOTX64.EFI", "EFI/BOOT/BOOTX64.EFI");
    /// ```
    ///
    /// Files come from the release's `v<version>-binary` tag on GitHub.
    /// To upgrade, change the version and the hashes.
    pub fn limine(&mut self, version: &str) -> LimineBuilder<'_> {
        LimineBuilder {
            cfg: self,
            version: version.trim_start_matches('v').to_string(),
        }
    }
}

/// Chainable builder returned by [`Config::limine`].
pub struct LimineBuilder<'cfg> {
    cfg: &'cfg mut Config,
    version: String,
}

impl LimineBuilder<'_> {
    /// Declare `file` from the release's binary tag (for example
    /// `BOOTX64.EFI`), pinned to `sha256`.
    pub fn file(self, file: &str, sha256: &str) -> Self {
        let url = format!("{LIMINE_BINARY_URL}/v{}-binary/{file}", self.version);
        self.cfg.download(&format!("limine/{file}"), &url, sha256);
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    const HASH: &str = "333f7a69379b1f47e019be215885cdc078b9edd6e9255cbe2f706d6c5ec0ef06";

    #[test]
    fn limine_file_declares_a_download_from_the_binary_tag() {
        let mut cfg = Config::new();
        cfg.limine("11.4.1")
            .file("BOOTX64.EFI", &HASH.to_uppercase());
        let h = cfg.raw.downloads.lookup("limine/BOOTX64.EFI").unwrap();
        let d = cfg.raw.downloads.get(h).unwrap();
        assert_eq!(
            d.url,
            "https://raw.githubusercontent.com/Limine-Bootloader/Limine/v11.4.1-binary/BOOTX64.EFI"
        );
        assert_eq!(d.sha256, HASH);
        assert_eq!(d.file_name(), "BOOTX64.EFI");
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn empty_hash_is_accepted_and_malformed_hash_is_rejected() {
        let mut cfg = Config::new();
        cfg.download("a", "https://example.com/a", "");
        cfg.download("b", "https://example.com/b", "abc");
        assert!(cfg.raw.downloads.lookup("a").is_some());
        assert!(cfg.raw.downloads.lookup("b").is_none());
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(cfg.diagnostics[0].message.contains("64 hex digits"));
    }

    #[test]
    fn duplicate_download_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.download("a", "https://example.com/a", HASH);
        cfg.download("a", "https://example.com/b", HASH);
        assert!(cfg.diagnostics[0].message.contains("more than once"));
    }
}
