//! The fingerprint store: one freshness check and one record function
//! for every cacheable node.
//!
//! Every node that can skip its work builds a [`FreshnessQuery`] and
//! calls [`FingerprintStore::check`]. After the work succeeds, it calls
//! [`FingerprintStore::record`]. Records live one JSON file per key
//! under `build/.fingerprint/`, so a node never waits for another
//! node's records, and a failed or interrupted build keeps every record
//! written before the failure.
//!
//! See `docs/src/architecture/cache.md` for the rule and its reasons.
//!
//! ### Freshness rule
//!
//! A record is fresh if and only if, in order:
//!
//! 1. it exists and its format is [`FORMAT`];
//! 2. its command hash and toolchain hash equal the query's;
//! 3. every output in the query exists;
//! 4. its declared input set equals the query's;
//! 5. every declared and discovered input exists, carries no dirty mark
//!    (it was modified while the recorded run was in progress), and
//!    either has the recorded mtime and size or the recorded SHA-256;
//! 6. every recorded environment variable has the value the next run
//!    would see.
//!
//! The first failing step is the [`StaleReason`]. A SHA-256 match on a
//! changed mtime rewrites the record's stamp, so the next check takes
//! the mtime path.

use crate::cache::hash::{sha256_bytes, sha256_file};
use crate::compile::BuildLayout;
use crate::error::{Error, Result};
use crate::scheduler::NodeData;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Version of the record layout. A record with another version is
/// stale ([`StaleReason::FormatChanged`]); there is no converter, since
/// one build refills the record.
pub const FORMAT: u32 = 3;

/// Longest key prefix kept in a record's file name.
const MAX_STEM: usize = 80;

/// A file's state when its record was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub mtime_ns: u128,
    pub size: u64,
    #[serde(with = "hex32")]
    pub sha256: [u8; 32],
    /// The file's mtime was at or after the start of the recorded run,
    /// so the run may have read an older version. A dirty stamp never
    /// matches.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dirty: bool,
}

/// One record, as stored in `build/.fingerprint/<key>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub format: u32,
    pub key: String,
    /// Process nodes: the hash of program, argv, env and cwd. Other
    /// nodes: a hash of their definition.
    #[serde(with = "hex32")]
    pub command: [u8; 32],
    /// [`crate::compile::RustcInfo::toolchain_hash`].
    #[serde(with = "hex32")]
    pub toolchain: [u8; 32],
    /// Inputs the node knew before it ran.
    pub declared: BTreeMap<PathBuf, FileStamp>,
    /// Inputs the run reported (depfile entries), minus declared ones
    /// and files under an immutable root.
    pub discovered: BTreeMap<PathBuf, FileStamp>,
    /// Environment variables the run read and the values it saw.
    /// `None` means unset.
    pub env: BTreeMap<String, Option<String>>,
    pub outputs: Vec<PathBuf>,
    pub started_at_ns: u128,
    /// A `.stderr` sidecar holds the run's diagnostics.
    #[serde(default)]
    pub stderr: bool,
    /// Data the node hands its dependents, restored on a cache hit
    /// (a build script's parsed output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<NodeData>,
}

/// Input for [`FingerprintStore::check`].
#[derive(Debug, Clone, Copy)]
pub struct FreshnessQuery<'a> {
    /// One record per key. A node may own several (an ESP owns one
    /// per entry).
    pub key: &'a str,
    pub command: [u8; 32],
    pub toolchain: [u8; 32],
    /// Inputs known before the run. Must equal the recorded set.
    pub declared: &'a [PathBuf],
    /// Every file that must exist for a hit.
    pub outputs: &'a [PathBuf],
    /// Environment the node sets on its child. A recorded variable is
    /// compared against this value when present, else against Quon's
    /// own environment.
    pub command_env: &'a BTreeMap<OsString, OsString>,
}

/// Input for [`FingerprintStore::record`].
#[derive(Debug, Clone)]
pub struct RecordInput<'a> {
    pub key: &'a str,
    pub command: [u8; 32],
    pub toolchain: [u8; 32],
    pub declared: &'a [PathBuf],
    pub discovered: &'a [PathBuf],
    pub env: BTreeMap<String, Option<String>>,
    pub outputs: &'a [PathBuf],
    /// Taken just before the process spawned (or the work started).
    pub started_at: SystemTime,
    /// Diagnostics to replay on later cache hits. Empty for none.
    pub stderr: &'a [u8],
    pub data: Option<NodeData>,
}

/// Result of [`FingerprintStore::check`].
#[derive(Debug)]
pub enum Freshness {
    Fresh(Box<Fingerprint>),
    Stale(StaleReason),
}

/// Why a record is not fresh: the first rule step that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleReason {
    NoRecord,
    FormatChanged,
    CommandChanged,
    ToolchainChanged,
    OutputMissing(PathBuf),
    DeclaredSetChanged,
    InputChanged(PathBuf),
    InputMissing(PathBuf),
    InputTouchedDuringBuild(PathBuf),
    EnvChanged(String),
}

impl fmt::Display for StaleReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRecord => write!(f, "no fingerprint record"),
            Self::FormatChanged => write!(f, "fingerprint record format changed"),
            Self::CommandChanged => write!(f, "command changed"),
            Self::ToolchainChanged => write!(f, "toolchain changed"),
            Self::OutputMissing(p) => write!(f, "output {} missing", p.display()),
            Self::DeclaredSetChanged => write!(f, "set of declared inputs changed"),
            Self::InputChanged(p) => write!(f, "input {} changed", p.display()),
            Self::InputMissing(p) => write!(f, "input {} missing", p.display()),
            Self::InputTouchedDuringBuild(p) => {
                write!(
                    f,
                    "input {} was modified during the last build",
                    p.display()
                )
            }
            Self::EnvChanged(name) => write!(f, "environment variable {name} changed"),
        }
    }
}

/// The record directory plus the roots whose files are never stat'ed.
#[derive(Debug, Clone)]
pub struct FingerprintStore {
    dir: PathBuf,
    /// Files under these roots are immutable: Cargo extracts a registry
    /// package once and never changes it. Records leave them out, so a
    /// no-op build does not stat them. The package identity (name,
    /// version, registry source) is in the unit's metadata hash, and so
    /// in its key and command. Vendor directories are not immutable,
    /// because people patch vendored crates.
    immutable_roots: Vec<PathBuf>,
}

impl FingerprintStore {
    /// The store for `layout`: `<root>/.fingerprint/`, with Cargo's
    /// registry source cache as the immutable root.
    pub fn for_layout(layout: &BuildLayout) -> Self {
        Self {
            dir: layout.fingerprint_dir(),
            immutable_roots: cargo_registry_src().into_iter().collect(),
        }
    }

    /// A store at `dir` with explicit immutable roots.
    pub fn new(dir: impl Into<PathBuf>, immutable_roots: Vec<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            immutable_roots,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of `key`'s record file.
    pub fn record_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.json", file_stem(key)))
    }

    fn stderr_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.stderr", file_stem(key)))
    }

    /// Apply the freshness rule (module docs) to `q`.
    pub fn check(&self, q: &FreshnessQuery<'_>) -> Freshness {
        match self.check_inner(q) {
            Ok(fp) => Freshness::Fresh(fp),
            Err(reason) => Freshness::Stale(reason),
        }
    }

    fn check_inner(
        &self,
        q: &FreshnessQuery<'_>,
    ) -> std::result::Result<Box<Fingerprint>, StaleReason> {
        let path = self.record_path(q.key);
        let bytes = std::fs::read(&path).map_err(|_| StaleReason::NoRecord)?;
        // An unparsable record is treated like one in an old format:
        // stale, rebuilt, overwritten.
        let mut fp: Box<Fingerprint> =
            serde_json::from_slice(&bytes).map_err(|_| StaleReason::FormatChanged)?;
        if fp.format != FORMAT {
            return Err(StaleReason::FormatChanged);
        }
        // Two keys can share a file name only through a hash prefix
        // collision; treat it as no record.
        if fp.key != q.key {
            return Err(StaleReason::NoRecord);
        }
        if fp.command != q.command {
            return Err(StaleReason::CommandChanged);
        }
        if fp.toolchain != q.toolchain {
            return Err(StaleReason::ToolchainChanged);
        }
        for out in q.outputs {
            if !out.exists() {
                return Err(StaleReason::OutputMissing(out.clone()));
            }
        }
        let declared: BTreeSet<&Path> = q.declared.iter().map(PathBuf::as_path).collect();
        if declared.len() != fp.declared.len()
            || !declared.iter().all(|p| fp.declared.contains_key(*p))
        {
            return Err(StaleReason::DeclaredSetChanged);
        }
        let mut refreshed = false;
        for stamps in [&mut fp.declared, &mut fp.discovered] {
            for (path, stamp) in stamps.iter_mut() {
                refreshed |= check_stamp(path, stamp)?;
            }
        }
        for (name, recorded) in &fp.env {
            let current = match q.command_env.get(OsStr::new(name)) {
                Some(v) => v.to_str().map(str::to_owned),
                None => std::env::var(name).ok(),
            };
            if current != *recorded {
                return Err(StaleReason::EnvChanged(name.clone()));
            }
        }
        if refreshed {
            // Best effort: a failed rewrite only costs a rehash next
            // time.
            let _ = self.write_record(&fp);
        }
        Ok(fp)
    }

    /// Write the record for a finished run and return it.
    ///
    /// Call after every output is complete (post steps included) and
    /// every output is parsed. An error leaves the previous record (if
    /// any) in place, which is stale for the new command, so the next
    /// build reruns the node.
    pub fn record(&self, r: RecordInput<'_>) -> Result<Fingerprint> {
        let mut declared = BTreeMap::new();
        for p in r.declared {
            declared.insert(p.clone(), stamp(p, r.started_at)?);
        }
        let mut discovered = BTreeMap::new();
        for p in r.discovered {
            if declared.contains_key(p)
                || discovered.contains_key(p)
                || self.immutable_roots.iter().any(|root| p.starts_with(root))
            {
                continue;
            }
            discovered.insert(p.clone(), stamp(p, r.started_at)?);
        }
        let fp = Fingerprint {
            format: FORMAT,
            key: r.key.to_string(),
            command: r.command,
            toolchain: r.toolchain,
            declared,
            discovered,
            env: r.env,
            outputs: r.outputs.to_vec(),
            started_at_ns: nanos(r.started_at),
            stderr: !r.stderr.is_empty(),
            data: r.data,
        };
        create_dir(&self.dir)?;
        // The sidecar goes first, so a record that says it has stderr
        // never points at a missing file.
        let sidecar = self.stderr_path(r.key);
        if r.stderr.is_empty() {
            match std::fs::remove_file(&sidecar) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&sidecar, e)),
            }
        } else {
            write_atomic(&sidecar, r.stderr)?;
        }
        self.write_record(&fp)?;
        Ok(fp)
    }

    /// The diagnostics recorded for `fp`, for replay on a cache hit.
    pub fn stderr(&self, fp: &Fingerprint) -> Vec<u8> {
        if !fp.stderr {
            return Vec::new();
        }
        std::fs::read(self.stderr_path(&fp.key)).unwrap_or_default()
    }

    fn write_record(&self, fp: &Fingerprint) -> Result<()> {
        let path = self.record_path(&fp.key);
        let bytes = serde_json::to_vec_pretty(fp)
            .map_err(|e| Error::Config(format!("serialising record {}: {e}", fp.key)))?;
        write_atomic(&path, &bytes)
    }
}

/// Check one input against its stamp. `Ok(true)` when the content
/// matched by hash and the stamp was refreshed.
fn check_stamp(path: &Path, stamp: &mut FileStamp) -> std::result::Result<bool, StaleReason> {
    if stamp.dirty {
        return Err(StaleReason::InputTouchedDuringBuild(path.to_path_buf()));
    }
    let meta =
        std::fs::metadata(path).map_err(|_| StaleReason::InputMissing(path.to_path_buf()))?;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .ok_or_else(|| StaleReason::InputChanged(path.to_path_buf()))?;
    if mtime_ns == stamp.mtime_ns && meta.len() == stamp.size {
        return Ok(false);
    }
    // mtime alone moves on `touch`, checkout and editors that save
    // without changes; the content decides.
    match sha256_file(path) {
        Ok(h) if h == stamp.sha256 => {
            stamp.mtime_ns = mtime_ns;
            stamp.size = meta.len();
            Ok(true)
        }
        _ => Err(StaleReason::InputChanged(path.to_path_buf())),
    }
}

/// Stamp `path` for a run that started at `started_at`.
fn stamp(path: &Path, started_at: SystemTime) -> Result<FileStamp> {
    let meta = std::fs::metadata(path).map_err(|e| io_err(path, e))?;
    let mtime = meta.modified().map_err(|e| io_err(path, e))?;
    Ok(FileStamp {
        mtime_ns: nanos(mtime),
        size: meta.len(),
        sha256: sha256_file(path)?,
        // Cargo's rule too. A filesystem with coarse mtimes can round
        // an edit made during the run down to before `started_at`;
        // that window is not covered.
        dirty: mtime >= started_at,
    })
}

fn nanos(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// File name stem for `key`: characters outside `[A-Za-z0-9._-]`
/// replaced by `_`, cut to [`MAX_STEM`] bytes, plus the first 8 hex
/// digits of the key's SHA-256 when anything was replaced or cut, so
/// two keys never share a file.
pub(crate) fn file_stem(key: &str) -> String {
    let mut stem: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let changed = stem != key || stem.len() > MAX_STEM;
    stem.truncate(MAX_STEM);
    if changed {
        stem.push('-');
        stem.push_str(&hex(&sha256_bytes(key.as_bytes()))[..8]);
    }
    stem
}

/// `$CARGO_HOME/registry/src`, with `$CARGO_HOME` defaulting to
/// `~/.cargo` as in Cargo.
fn cargo_registry_src() -> Option<PathBuf> {
    Some(
        crate::manifest::sources::cargo_home()?
            .join("registry")
            .join("src"),
    )
}

fn create_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))
}

/// Write via a temp file and rename, so a reader never sees a partial
/// record. No fsync: after a crash a record is either the old one, the
/// new one, or unparsable, and an unparsable record is stale.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(tmp);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(&tmp, e));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io_err(path, e)
    })
}

fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `[u8; 32]` as a 64-digit hex string, so records stay readable.
mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::hex(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = <&str>::deserialize(d)?;
        if s.len() != 64 {
            return Err(D::Error::custom("expected 64 hex digits"));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(D::Error::custom)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::time::Duration;

    struct Fx {
        _tmp: tempfile::TempDir,
        src: PathBuf,
        out: PathBuf,
        store: FingerprintStore,
    }

    static NO_ENV: BTreeMap<OsString, OsString> = BTreeMap::new();

    impl Fx {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let src = tmp.path().join("src");
            fs::create_dir_all(&src).unwrap();
            let out = tmp.path().join("out.rlib");
            fs::write(&out, b"out").unwrap();
            let store = FingerprintStore::new(
                tmp.path().join(".fingerprint"),
                vec![tmp.path().join("registry")],
            );
            Self {
                _tmp: tmp,
                src,
                out,
                store,
            }
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let p = self.src.join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, bytes).unwrap();
            p
        }

        fn outputs(&self) -> Vec<PathBuf> {
            vec![self.out.clone()]
        }

        fn record(&self, declared: &[PathBuf], discovered: &[PathBuf]) {
            self.record_with(declared, discovered, BTreeMap::new(), b"");
        }

        fn record_with(
            &self,
            declared: &[PathBuf],
            discovered: &[PathBuf],
            env: BTreeMap<String, Option<String>>,
            stderr: &[u8],
        ) {
            self.store
                .record(RecordInput {
                    key: "k",
                    command: [1; 32],
                    toolchain: [2; 32],
                    declared,
                    discovered,
                    env,
                    outputs: &self.outputs(),
                    // After every file the test wrote.
                    started_at: SystemTime::now(),
                    stderr,
                    data: None,
                })
                .unwrap();
        }

        fn check(&self, declared: &[PathBuf]) -> Freshness {
            self.check_with(declared, [1; 32], [2; 32], &NO_ENV)
        }

        fn check_with(
            &self,
            declared: &[PathBuf],
            command: [u8; 32],
            toolchain: [u8; 32],
            command_env: &BTreeMap<OsString, OsString>,
        ) -> Freshness {
            self.store.check(&FreshnessQuery {
                key: "k",
                command,
                toolchain,
                declared,
                outputs: &self.outputs(),
                command_env,
            })
        }
    }

    fn stale(f: Freshness) -> StaleReason {
        match f {
            Freshness::Stale(r) => r,
            Freshness::Fresh(_) => panic!("expected stale"),
        }
    }

    fn fresh(f: Freshness) -> Fingerprint {
        match f {
            Freshness::Fresh(fp) => *fp,
            Freshness::Stale(r) => panic!("expected fresh, got stale: {r}"),
        }
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn recorded_node_is_fresh_and_survives_reload() {
        let fx = Fx::new();
        let a = fx.file("a.rs", b"a");
        let b = fx.file("b.rs", b"b");
        fx.record(std::slice::from_ref(&a), std::slice::from_ref(&b));
        let fp = fresh(fx.check(std::slice::from_ref(&a)));
        assert!(fp.declared.contains_key(&a));
        assert!(fp.discovered.contains_key(&b));
    }

    #[test]
    fn no_record() {
        let fx = Fx::new();
        assert_eq!(stale(fx.check(&[])), StaleReason::NoRecord);
    }

    #[test]
    fn format_changed() {
        let fx = Fx::new();
        fx.record(&[], &[]);
        let path = fx.store.record_path("k");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replace("\"format\": 3", "\"format\": 2")).unwrap();
        assert_eq!(stale(fx.check(&[])), StaleReason::FormatChanged);
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(stale(fx.check(&[])), StaleReason::FormatChanged);
    }

    #[test]
    fn command_and_toolchain_changed() {
        let fx = Fx::new();
        fx.record(&[], &[]);
        assert_eq!(
            stale(fx.check_with(&[], [9; 32], [2; 32], &NO_ENV)),
            StaleReason::CommandChanged
        );
        assert_eq!(
            stale(fx.check_with(&[], [1; 32], [9; 32], &NO_ENV)),
            StaleReason::ToolchainChanged
        );
    }

    #[test]
    fn output_missing() {
        let fx = Fx::new();
        fx.record(&[], &[]);
        fs::remove_file(&fx.out).unwrap();
        assert_eq!(
            stale(fx.check(&[])),
            StaleReason::OutputMissing(fx.out.clone())
        );
    }

    #[test]
    fn declared_set_changed() {
        let fx = Fx::new();
        let a = fx.file("a.rs", b"a");
        let b = fx.file("b.rs", b"b");
        fx.record(std::slice::from_ref(&a), &[]);
        assert_eq!(
            stale(fx.check(&[a.clone(), b.clone()])),
            StaleReason::DeclaredSetChanged
        );
        assert_eq!(stale(fx.check(&[])), StaleReason::DeclaredSetChanged);
        // A query that lists one path twice is the same set.
        fresh(fx.check(&[a.clone(), a]));
    }

    #[test]
    fn input_changed_and_missing() {
        let fx = Fx::new();
        let a = fx.file("a.rs", b"a");
        let b = fx.file("b.rs", b"b");
        fx.record(&[], &[a.clone(), b.clone()]);
        fs::write(&a, b"changed").unwrap();
        assert_eq!(stale(fx.check(&[])), StaleReason::InputChanged(a.clone()));
        fs::write(&a, b"a").unwrap();
        fs::remove_file(&b).unwrap();
        assert_eq!(stale(fx.check(&[])), StaleReason::InputMissing(b));
    }

    #[test]
    fn touched_input_matches_by_hash_and_refreshes_its_stamp() {
        let fx = Fx::new();
        let a = fx.file("a.rs", b"same");
        fx.record(&[], std::slice::from_ref(&a));
        let later = SystemTime::now() + Duration::from_secs(10);
        set_mtime(&a, later);
        let fp = fresh(fx.check(&[]));
        assert_eq!(fp.discovered[&a].mtime_ns, nanos(later));
        // The refresh is on disk, so the next check needs no hash.
        let on_disk: Fingerprint =
            serde_json::from_slice(&fs::read(fx.store.record_path("k")).unwrap()).unwrap();
        assert_eq!(on_disk.discovered[&a].mtime_ns, nanos(later));
    }

    #[test]
    fn input_touched_during_build_is_stale_next_time() {
        let fx = Fx::new();
        let a = fx.file("a.rs", b"a");
        // The file's mtime is after the run started: the run may have
        // read the old content.
        fx.store
            .record(RecordInput {
                key: "k",
                command: [1; 32],
                toolchain: [2; 32],
                declared: &[],
                discovered: std::slice::from_ref(&a),
                env: BTreeMap::new(),
                outputs: &fx.outputs(),
                started_at: SystemTime::now() - Duration::from_secs(10),
                stderr: b"",
                data: None,
            })
            .unwrap();
        set_mtime(&a, SystemTime::now());
        fx.store
            .record(RecordInput {
                key: "k",
                command: [1; 32],
                toolchain: [2; 32],
                declared: &[],
                discovered: std::slice::from_ref(&a),
                env: BTreeMap::new(),
                outputs: &fx.outputs(),
                started_at: SystemTime::now() - Duration::from_secs(1),
                stderr: b"",
                data: None,
            })
            .unwrap();
        assert_eq!(
            stale(fx.check(&[])),
            StaleReason::InputTouchedDuringBuild(a)
        );
    }

    #[test]
    fn env_compares_against_command_env_then_process_env() {
        let fx = Fx::new();
        // A name no test environment sets.
        let name = "QUON_STORE_TEST_VAR_NEVER_SET";
        let env: BTreeMap<String, Option<String>> =
            [(name.to_string(), Some("one".to_string()))].into();
        fx.record_with(&[], &[], env, b"");
        let cmd_env = |v: &str| -> BTreeMap<OsString, OsString> {
            [(OsString::from(name), OsString::from(v))].into()
        };
        fresh(fx.check_with(&[], [1; 32], [2; 32], &cmd_env("one")));
        assert_eq!(
            stale(fx.check_with(&[], [1; 32], [2; 32], &cmd_env("two"))),
            StaleReason::EnvChanged(name.into())
        );
        // Not in the command env: Quon's environment, where it is unset.
        assert_eq!(stale(fx.check(&[])), StaleReason::EnvChanged(name.into()));

        let unset: BTreeMap<String, Option<String>> = [(name.to_string(), None)].into();
        fx.record_with(&[], &[], unset, b"");
        fresh(fx.check(&[]));
        assert_eq!(
            stale(fx.check_with(&[], [1; 32], [2; 32], &cmd_env(""))),
            StaleReason::EnvChanged(name.into()),
            "unset and empty are different values"
        );
    }

    #[test]
    fn dep_rebuilt_with_identical_content_keeps_dependent_fresh() {
        let fx = Fx::new();
        let dep = fx.file("libdep.rlib", b"dep v1");
        fx.record(std::slice::from_ref(&dep), &[]);
        // The dep reran and wrote the same bytes: new mtime, same hash.
        fs::write(&dep, b"dep v1").unwrap();
        set_mtime(&dep, SystemTime::now() + Duration::from_secs(5));
        fresh(fx.check(std::slice::from_ref(&dep)));
        fs::write(&dep, b"dep v2").unwrap();
        assert_eq!(
            stale(fx.check(std::slice::from_ref(&dep))),
            StaleReason::InputChanged(dep)
        );
    }

    #[test]
    fn files_under_an_immutable_root_are_not_recorded() {
        let fx = Fx::new();
        let reg = fx
            .src
            .parent()
            .unwrap()
            .join("registry/src/foo-1.0.0/lib.rs");
        fs::create_dir_all(reg.parent().unwrap()).unwrap();
        fs::write(&reg, b"v1").unwrap();
        let vendored = fx.file("vendor/foo-1.0.0/lib.rs", b"v1");
        fx.record(&[], &[reg.clone(), vendored.clone()]);
        let fp = fresh(fx.check(&[]));
        assert!(!fp.discovered.contains_key(&reg));
        // An edit in the registry cache is not looked at; one in a
        // vendor directory is.
        fs::write(&reg, b"edited").unwrap();
        fresh(fx.check(&[]));
        fs::write(&vendored, b"patched").unwrap();
        assert_eq!(stale(fx.check(&[])), StaleReason::InputChanged(vendored));
    }

    #[test]
    fn stderr_is_kept_for_replay_and_cleared_by_a_quiet_run() {
        let fx = Fx::new();
        fx.record_with(&[], &[], BTreeMap::new(), b"warning: x\n");
        let fp = fresh(fx.check(&[]));
        assert_eq!(fx.store.stderr(&fp), b"warning: x\n");
        fx.record(&[], &[]);
        let fp = fresh(fx.check(&[]));
        assert!(fx.store.stderr(&fp).is_empty());
        let leftovers: Vec<_> = fs::read_dir(fx.store.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "sidecar or temp file left: {leftovers:?}"
        );
    }

    #[test]
    fn declared_inputs_are_not_duplicated_in_discovered() {
        let fx = Fx::new();
        let a = fx.file("a.rs", b"a");
        fx.record(std::slice::from_ref(&a), &[a.clone(), a.clone()]);
        let fp = fresh(fx.check(std::slice::from_ref(&a)));
        assert!(fp.discovered.is_empty());
    }

    #[test]
    fn file_stems_are_safe_bounded_and_distinct() {
        assert_eq!(file_stem("sysroot-x86"), "sysroot-x86");
        let a = file_stem("crate:cross:kernel:dev:x86_64");
        assert!(a.starts_with("crate_cross_kernel_dev_x86_64-"), "{a}");
        assert_ne!(a, file_stem("crate_cross_kernel_dev_x86_64"));
        let long = file_stem(&"x".repeat(200));
        assert_eq!(long.len(), MAX_STEM + 9);
        assert_ne!(long, file_stem(&"x".repeat(201)));
        assert!(file_stem("ü/../é").bytes().all(|b| b.is_ascii()));
    }
}
