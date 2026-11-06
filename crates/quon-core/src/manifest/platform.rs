//! Target cfg probe and `[target.'...'.dependencies]` matching.
//!
//! Cargo selects target-conditional dependencies by matching each
//! `cfg(...)` or triple key against the cfg set that `rustc --print cfg`
//! reports for the target. Quon does the same, so it needs the same
//! cfg set. [`probe`] runs rustc with the same `--target` argument the
//! command builder emits for the target (builtin triple, or
//! `-Zunstable-options --target <spec.json>`), or no `--target` for the
//! host.
//!
//! # Cache and freshness rule
//!
//! Probing costs one rustc spawn (about 20 ms) per target per run, so
//! results are cached in `build/target-cfg.json`. An entry is keyed by
//! a SHA-256 over:
//!
//! - the rustc path, its `-vV` identity (version line, commit hash,
//!   release, host) and its mtime, so a toolchain change re-probes;
//! - the builtin triple, or the SHA-256 of the custom spec file's
//!   contents, so editing a spec re-probes.
//!
//! A missing, unreadable or corrupt cache file is a miss, never an
//! error. A failed write is ignored: the next run probes again.
//!
//! `debug_assertions` is dropped from the probed set. Cargo removes it
//! for dependency selection because it depends on the profile, not the
//! target.

use crate::compile::RustcInfo;
use crate::error::{Error, Result};
use cargo_platform::{Cfg, Platform};
use quon_model::TargetDef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

/// What a cfg probe runs against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTarget {
    /// The build machine: rustc's default target.
    Host,
    /// A rustc builtin triple.
    Builtin(String),
    /// An absolute path to a custom target spec JSON file.
    Spec(PathBuf),
}

impl ProbeTarget {
    /// The probe target for a Quon target. A relative spec path is
    /// resolved against `project_root`, which is where `quon.rs`
    /// relative paths are anchored.
    pub fn for_target(target: &TargetDef, project_root: &Path) -> Self {
        if target.builtin {
            ProbeTarget::Builtin(target.spec.clone())
        } else {
            ProbeTarget::Spec(project_root.join(&target.spec))
        }
    }
}

/// The cfg set of one target, as Cargo uses it for dependency selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPlatform {
    /// The name a `[target.<triple>]` key matches: the builtin triple,
    /// the custom spec's file stem (`x86_64-hadron` for
    /// `targets/x86_64-hadron.json`, as Cargo does), or the host triple.
    pub name: String,
    /// `rustc --print cfg` output in rustc's order, without
    /// `debug_assertions`.
    pub cfgs: Vec<Cfg>,
}

impl TargetPlatform {
    /// Build a platform from raw `rustc --print cfg` lines.
    pub fn from_cfg_lines<'a>(
        name: impl Into<String>,
        lines: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self> {
        let mut cfgs = Vec::new();
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let cfg = Cfg::from_str(line).map_err(|e| {
                Error::Config(format!(
                    "cannot parse `rustc --print cfg` line `{line}`: {e}"
                ))
            })?;
            if matches!(&cfg, Cfg::Name(n) if n.as_str() == "debug_assertions") {
                continue;
            }
            cfgs.push(cfg);
        }
        Ok(Self {
            name: name.into(),
            cfgs,
        })
    }

    /// Whether a parsed `[target.'...']` key applies to this platform.
    pub fn matches(&self, platform: &Platform) -> bool {
        platform.matches(&self.name, &self.cfgs)
    }

    /// Parse a `[target.'...']` key (a `cfg(...)` expression or a target
    /// name) and match it against this platform.
    pub fn matches_key(&self, key: &str) -> Result<bool> {
        let platform = parse_platform(key)?;
        Ok(self.matches(&platform))
    }
}

/// Parse a `[target.'...']` key or a `dep_kinds[].target` string.
pub fn parse_platform(key: &str) -> Result<Platform> {
    Platform::from_str(key)
        .map_err(|e| Error::Config(format!("invalid target platform `{key}`: {e}")))
}

/// On-disk form of the probe cache. A `BTreeMap` keeps the file
/// byte-stable across runs.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    entries: BTreeMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    name: String,
    lines: Vec<String>,
}

/// Probe the cfg set of `target`, consulting the cache at `cache_path`
/// when one is given. See the module docs for the freshness rule.
pub fn probe(
    rustc: &RustcInfo,
    target: &ProbeTarget,
    cache_path: Option<&Path>,
) -> Result<TargetPlatform> {
    let key = cache_key(rustc, target)?;
    let mut cache = cache_path.map(load_cache).unwrap_or_default();
    if let Some(entry) = cache.entries.get(&key)
        && let Ok(p) = TargetPlatform::from_cfg_lines(&entry.name, entry.lines.iter().map(|s| &**s))
    {
        return Ok(p);
    }

    let lines = run_print_cfg(rustc, target)?;
    let name = platform_name(rustc, target);
    let platform = TargetPlatform::from_cfg_lines(&name, lines.iter().map(|s| &**s))?;

    if let Some(path) = cache_path {
        cache.entries.insert(key, CacheEntry { name, lines });
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec_pretty(&cache) {
            let _ = std::fs::write(path, json);
        }
    }
    Ok(platform)
}

fn load_cache(path: &Path) -> CacheFile {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn platform_name(rustc: &RustcInfo, target: &ProbeTarget) -> String {
    match target {
        ProbeTarget::Host => rustc.host_triple.clone(),
        ProbeTarget::Builtin(triple) => triple.clone(),
        ProbeTarget::Spec(path) => path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

fn cache_key(rustc: &RustcInfo, target: &ProbeTarget) -> Result<String> {
    let mut h = Sha256::new();
    h.update(b"quon.target-cfg.v1\0");
    for part in [
        rustc.rustc_path.to_string_lossy().as_bytes(),
        rustc.version.as_bytes(),
        rustc.commit_hash.as_deref().unwrap_or("").as_bytes(),
        rustc.release.as_bytes(),
        rustc.host_triple.as_bytes(),
        rustc.mtime_ns.to_string().as_bytes(),
    ] {
        h.update(part);
        h.update(b"\0");
    }
    match target {
        ProbeTarget::Host => h.update(b"host"),
        ProbeTarget::Builtin(triple) => {
            h.update(b"builtin\0");
            h.update(triple.as_bytes());
        }
        ProbeTarget::Spec(path) => {
            let bytes = std::fs::read(path).map_err(|e| Error::Io {
                path: path.clone(),
                source: e,
            })?;
            h.update(b"spec\0");
            h.update(path.file_stem().unwrap_or_default().as_encoded_bytes());
            h.update(b"\0");
            h.update(Sha256::digest(&bytes));
        }
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn run_print_cfg(rustc: &RustcInfo, target: &ProbeTarget) -> Result<Vec<String>> {
    let mut cmd = Command::new(&rustc.rustc_path);
    cmd.args(["--print", "cfg"]);
    // Same `--target` shape as `RustcCommandBuilder::target`, so the
    // probe sees exactly what the compile sees.
    match target {
        ProbeTarget::Host => {}
        ProbeTarget::Builtin(triple) => {
            cmd.arg(format!("--target={triple}"));
        }
        ProbeTarget::Spec(path) => {
            cmd.arg("-Zunstable-options").arg("--target").arg(path);
        }
    }
    let shown = format!("{cmd:?}");
    let out = cmd
        .output()
        .map_err(|e| Error::Config(format!("failed to spawn {shown}: {e}")))?;
    if !out.status.success() {
        return Err(Error::Config(format!(
            "{shown} exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8(out.stdout)
        .map_err(|e| Error::Config(format!("{shown} produced non-UTF-8 output: {e}")))?;
    Ok(text.lines().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none_platform() -> TargetPlatform {
        TargetPlatform::from_cfg_lines(
            "x86_64-unknown-none",
            [
                "debug_assertions",
                "panic=\"abort\"",
                "target_arch=\"x86_64\"",
                "target_os=\"none\"",
                "target_pointer_width=\"64\"",
            ],
        )
        .unwrap()
    }

    #[test]
    fn debug_assertions_is_dropped() {
        let p = none_platform();
        assert!(!p.matches_key("cfg(debug_assertions)").unwrap());
        assert_eq!(p.cfgs.len(), 4);
    }

    #[test]
    fn cfg_expressions_match() {
        let p = none_platform();
        assert!(p.matches_key("cfg(target_os = \"none\")").unwrap());
        assert!(!p.matches_key("cfg(unix)").unwrap());
        assert!(p.matches_key("cfg(not(unix))").unwrap());
        assert!(
            p.matches_key("cfg(all(target_arch = \"x86_64\", target_pointer_width = \"64\"))")
                .unwrap()
        );
    }

    #[test]
    fn triple_keys_match_by_name() {
        let p = none_platform();
        assert!(p.matches_key("x86_64-unknown-none").unwrap());
        assert!(!p.matches_key("aarch64-unknown-none").unwrap());
    }

    #[test]
    fn invalid_key_errors() {
        assert!(none_platform().matches_key("cfg(unix").is_err());
    }

    #[test]
    fn spec_name_is_file_stem() {
        let info = RustcInfo {
            rustc_path: PathBuf::from("/opt/rust/bin/rustc"),
            rustc_arg: "rustc".into(),
            version: "rustc 1.0.0".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: None,
            release: "1.0.0".into(),
            sysroot: PathBuf::from("/opt/rust"),
            rust_src: None,
            mtime_ns: 0,
        };
        let t = ProbeTarget::Spec(PathBuf::from("/x/targets/x86_64-hadron.json"));
        assert_eq!(platform_name(&info, &t), "x86_64-hadron");
        assert_eq!(platform_name(&info, &ProbeTarget::Host), info.host_triple);
    }

    #[test]
    fn probe_builtin_and_host_with_cache() {
        let Ok(info) = RustcInfo::probe() else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("target-cfg.json");
        let target = ProbeTarget::Builtin("x86_64-unknown-none".into());
        let first = probe(&info, &target, Some(&cache)).expect("probe");
        assert!(first.matches_key("cfg(target_os = \"none\")").unwrap());
        assert!(cache.exists());
        let second = probe(&info, &target, Some(&cache)).expect("cached probe");
        assert_eq!(first, second);

        let host = probe(&info, &ProbeTarget::Host, Some(&cache)).expect("host probe");
        assert_eq!(host.name, info.host_triple);
        let file: CacheFile = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        assert_eq!(file.entries.len(), 2);
    }

    #[test]
    fn probe_custom_spec_uses_file_stem() {
        let Ok(info) = RustcInfo::probe() else {
            return;
        };
        // Generate a spec this rustc accepts instead of checking one in:
        // the spec JSON schema changes between nightlies.
        let out = Command::new(&info.rustc_path)
            .args(["-Zunstable-options", "--print", "target-spec-json"])
            .arg("--target=x86_64-unknown-none")
            .output()
            .unwrap();
        if !out.status.success() {
            return; // stable toolchain: no custom specs
        }
        let tmp = tempfile::tempdir().unwrap();
        let spec = tmp.path().join("x86_64-custom.json");
        std::fs::write(&spec, &out.stdout).unwrap();
        let p = probe(&info, &ProbeTarget::Spec(spec), None).expect("probe spec");
        assert_eq!(p.name, "x86_64-custom");
        assert!(p.matches_key("x86_64-custom").unwrap());
        assert!(p.matches_key("cfg(target_os = \"none\")").unwrap());
    }

    #[test]
    fn spec_edit_changes_cache_key() {
        let Ok(info) = RustcInfo::probe() else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let spec = tmp.path().join("t.json");
        std::fs::write(&spec, "{}").unwrap();
        let a = cache_key(&info, &ProbeTarget::Spec(spec.clone())).unwrap();
        std::fs::write(&spec, "{ }").unwrap();
        let b = cache_key(&info, &ProbeTarget::Spec(spec)).unwrap();
        assert_ne!(a, b);
    }
}
