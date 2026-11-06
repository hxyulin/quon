//! Pinned downloads: files fetched over HTTPS once and checked
//! against a SHA-256 written in `quon.rs`.
//!
//! A [`DownloadDef`] is declared with `cfg.download(...)` or the
//! `cfg.limine(...)` helper and consumed by ESP and image entries. The
//! ESP and image nodes call [`ensure_download`] when they resolve such
//! an entry, so nothing is fetched unless a declared download is used.
//!
//! ### Mechanism
//!
//! Quon runs `curl` rather than linking an HTTP client. Every
//! supported host ships curl, and a TLS stack would add more build time
//! and dependencies than the rest of `quon-core` together. The file
//! lands in a temporary name next to its cache path, is hashed, and is
//! renamed into place only when the hash matches. Two nodes fetching
//! the same file at once each use their own temporary name, and the
//! rename is atomic, so neither sees a partial file.
//!
//! ### Freshness
//!
//! The cache path is `<build>/downloads/<sha256>/<file name>`, so it is
//! keyed by content: changing the pinned hash (an upgrade) picks a new
//! path, and the ESP or image entry's fingerprint, which declares that
//! path as an input, goes stale. A cached file is hashed again on every
//! use and fetched again when it no longer matches. The files are a
//! few hundred KiB, so the re-hash costs less than a millisecond.

use crate::compile::BuildLayout;
use crate::error::{Error, Result};
use quon_model::DownloadDef;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Cache path of `def`'s verified file.
pub fn download_path(layout: &BuildLayout, def: &DownloadDef) -> PathBuf {
    layout
        .downloads_dir()
        .join(def.sha256.to_ascii_lowercase())
        .join(def.file_name())
}

/// Return the path of `def`'s file and whether it was fetched now,
/// fetching it unless a copy with the pinned hash is already cached.
///
/// Errors when the hash is empty or does not match. Both errors print
/// the hash of the fetched file, so the user can pin it after checking
/// where it came from.
pub fn ensure_download(layout: &BuildLayout, def: &DownloadDef) -> Result<(PathBuf, bool)> {
    let expected = def.sha256.to_ascii_lowercase();
    let path = download_path(layout, def);
    if !expected.is_empty() && sha256_file(&path).ok().as_deref() == Some(expected.as_str()) {
        return Ok((path, false));
    }

    let dir = path.parent().expect("download path has a parent");
    std::fs::create_dir_all(dir).map_err(|e| Error::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    let part = temp_path(&path);
    let fetched = fetch(&def.url, &part).and_then(|()| {
        sha256_file(&part).map_err(|e| Error::Io {
            path: part.clone(),
            source: e,
        })
    });
    let actual = match fetched {
        Ok(actual) => actual,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(download_error(def, e.to_string()));
        }
    };
    if actual != expected {
        let _ = std::fs::remove_file(&part);
        let message = if expected.is_empty() {
            format!(
                "no SHA-256 is pinned. The file at {} has SHA-256 {actual}; \
                 check where it came from, then add that hash to quon.rs",
                def.url
            )
        } else {
            format!(
                "SHA-256 mismatch for {}: quon.rs pins {expected}, the download has {actual}",
                def.url
            )
        };
        return Err(download_error(def, message));
    }
    std::fs::rename(&part, &path).map_err(|e| Error::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok((path, true))
}

fn download_error(def: &DownloadDef, message: String) -> Error {
    Error::Download {
        name: def.name.clone(),
        message,
    }
}

/// A temporary name in the cache directory, unique per process and
/// call, so concurrent fetches never write the same file.
fn temp_path(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.part", std::process::id()));
    path.with_file_name(name)
}

/// Fetch `url` into `dest` with curl. `--proto =https,file` refuses
/// plain HTTP, including on redirects, and keeps `file://` URLs for
/// local mirrors; `--fail` turns an HTTP error status into a non-zero
/// exit instead of saving the error page.
fn fetch(url: &str, dest: &Path) -> Result<()> {
    let output = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args([
            "--proto",
            "=https,file",
            "--retry",
            "2",
            "--connect-timeout",
            "20",
        ])
        .arg("--output")
        .arg(dest)
        .arg(url)
        .output()
        .map_err(|e| {
            Error::Config(format!(
                "could not run curl, which Quon uses to fetch {url}: {e}"
            ))
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "curl failed to fetch {url} ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

/// Lowercase hex SHA-256 of the file at `path`.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn def(url: String, sha256: &str) -> DownloadDef {
        DownloadDef {
            name: "test/hello.bin".into(),
            url,
            sha256: sha256.into(),
            span: None,
        }
    }

    /// A `file://` URL serves as an offline mirror; curl is on every
    /// supported host.
    fn local_source(dir: &Path) -> String {
        let src = dir.join("hello.bin");
        std::fs::write(&src, b"hello").unwrap();
        format!("file://{}", src.display())
    }

    #[test]
    fn fetches_once_then_serves_the_cached_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = BuildLayout::new(tmp.path().join("build"), "t");
        let d = def(local_source(tmp.path()), &HELLO_SHA256.to_uppercase());

        let (path, fetched) = ensure_download(&layout, &d).unwrap();
        assert!(fetched);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(path.ends_with(format!("downloads/{HELLO_SHA256}/hello.bin")));

        std::fs::remove_file(tmp.path().join("hello.bin")).unwrap();
        let (again, fetched) = ensure_download(&layout, &d).unwrap();
        assert_eq!(again, path);
        assert!(
            !fetched,
            "a cached copy with the pinned hash needs no fetch"
        );
    }

    #[test]
    fn corrupted_cache_is_fetched_again() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = BuildLayout::new(tmp.path().join("build"), "t");
        let d = def(local_source(tmp.path()), HELLO_SHA256);
        let (path, _) = ensure_download(&layout, &d).unwrap();
        std::fs::write(&path, b"tampered").unwrap();
        let (_, fetched) = ensure_download(&layout, &d).unwrap();
        assert!(fetched);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn mismatch_prints_both_hashes_and_caches_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = BuildLayout::new(tmp.path().join("build"), "t");
        let wrong = "0".repeat(64);
        let d = def(local_source(tmp.path()), &wrong);
        let err = ensure_download(&layout, &d).unwrap_err().to_string();
        assert!(err.contains("test/hello.bin"), "{err}");
        assert!(err.contains(&wrong), "{err}");
        assert!(err.contains(HELLO_SHA256), "{err}");
        let dir = download_path(&layout, &d).parent().unwrap().to_path_buf();
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
    }

    #[test]
    fn missing_hash_prints_the_actual_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = BuildLayout::new(tmp.path().join("build"), "t");
        let d = def(local_source(tmp.path()), "");
        let err = ensure_download(&layout, &d).unwrap_err().to_string();
        assert!(err.contains("no SHA-256 is pinned"), "{err}");
        assert!(err.contains(HELLO_SHA256), "{err}");
    }

    #[test]
    fn fetch_failure_names_the_download() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = BuildLayout::new(tmp.path().join("build"), "t");
        let url = format!("file://{}", tmp.path().join("absent.bin").display());
        let err = ensure_download(&layout, &def(url, HELLO_SHA256))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("test/hello.bin") && err.contains("curl"),
            "{err}"
        );
    }
}
