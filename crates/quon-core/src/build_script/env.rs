//! Assemble the environment a build script runs with.
//!
//! The child inherits quon's environment minus the variables
//! [`should_remove_inherited`] matches, then gets the variables
//! [`script_env`] returns. The split matters for freshness: only the
//! variables quon sets go into the run's cache key, so an unrelated
//! change in the user's shell does not rerun every script. Scripts that
//! read other variables declare them with `rerun-if-env-changed`.
//!
//! Everything here is a pure function of its inputs. Callers pass the
//! inherited variable names and the target cfgs in, so tests never touch
//! the process environment.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::compile::{HOST_DEBUG_INFO, HOST_OPT_LEVEL};

/// Package metadata for `CARGO_PKG_*` and `CARGO_MANIFEST_*`.
///
/// Optional fields are `None` when the manifest omits them; they are
/// still set, to the empty string, as Cargo does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: String,
    /// Full semver string, e.g. `1.2.3-beta.1+build.5`.
    pub version: String,
    pub authors: Vec<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub rust_version: Option<String>,
    pub readme: Option<String>,
    /// The `links` key, if any.
    pub links: Option<String>,
    pub manifest_dir: PathBuf,
    pub manifest_path: PathBuf,
}

/// The target a cross unit is compiled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptTarget<'a> {
    /// A triple rustc knows, e.g. `x86_64-unknown-none`.
    Builtin(&'a str),
    /// A custom target spec file, e.g. `targets/x86_64-hadron.json`.
    Custom(&'a Path),
}

/// The platform of the unit that owns the script. The script itself
/// always runs on the host; this is what it is configuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptPlatform<'a> {
    /// A host unit (a proc macro or one of its deps). Uses the fixed
    /// host profile ([`HOST_OPT_LEVEL`], [`HOST_DEBUG_INFO`]).
    Host,
    /// A unit built for a cross target with quon's sysroot.
    Cross {
        target: ScriptTarget<'a>,
        /// Quon's sysroot for this target. Passed to probes through
        /// `CARGO_ENCODED_RUSTFLAGS`.
        quon_sysroot: &'a Path,
        opt_level: quon_model::OptLevel,
        debug_info: bool,
    },
}

/// Inputs to [`script_env`].
#[derive(Debug, Clone, Copy)]
pub struct ScriptEnvInput<'a> {
    pub package: &'a PackageInfo,
    /// Enabled features of the owning unit.
    pub features: &'a [String],
    pub platform: ScriptPlatform<'a>,
    /// The run's `OUT_DIR`.
    pub out_dir: &'a Path,
    /// `RustcInfo::host_triple`.
    pub host_triple: &'a str,
    /// `RustcInfo::sysroot`: the toolchain quon uses, not quon's
    /// per-target sysroot. `RUSTC` and `RUSTDOC` point into it.
    pub toolchain_sysroot: &'a Path,
    /// The cargo binary quon resolves for vendoring.
    pub cargo: &'a Path,
    /// Quon's worker count.
    pub num_jobs: usize,
    /// `rustc --print cfg` for the unit's platform and profile, as
    /// `(name, value)` pairs in rustc's output order: `unix` is
    /// `("unix", None)`, `target_os="none"` is `("target_os",
    /// Some("none"))`.
    pub target_cfgs: &'a [(String, Option<String>)],
    /// Flags quon passes to every crate of the platform (code model,
    /// target features, panic strategy). Per-crate flags are excluded.
    pub target_rustflags: &'a [String],
    /// Metadata from each direct dep whose package declares `links`, as
    /// `(links value, metadata pairs)`.
    pub dep_metadata: &'a [(String, Vec<(String, String)>)],
}

/// Whether an inherited variable must be removed before the script
/// runs.
///
/// - `CARGO_*`: quon may itself run under `cargo run`, which sets
///   `CARGO_PKG_NAME=quon` and friends. A script must see only the
///   values quon sets for its own package.
/// - `DEP_*`: these come only from the script's direct deps.
/// - `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`: quon does not wrap its
///   own rustc calls, so probe compiles must not be wrapped either.
/// - `CARGO_MAKEFLAGS`: quon offers no jobserver. (Covered by the
///   `CARGO_` prefix; listed for the reader.)
/// - `RUSTFLAGS`: quon passes its flags in `CARGO_ENCODED_RUSTFLAGS`,
///   and a script that read the user's `RUSTFLAGS` would probe with
///   flags the crate is not built with.
pub fn should_remove_inherited(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.starts_with("CARGO_")
        || name.starts_with("DEP_")
        || matches!(
            name,
            "RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER" | "CARGO_MAKEFLAGS" | "RUSTFLAGS"
        )
}

/// Variables quon sets for one build-script run, sorted by name.
pub fn script_env(input: &ScriptEnvInput<'_>) -> BTreeMap<String, OsString> {
    let mut env: BTreeMap<String, OsString> = BTreeMap::new();
    let mut set = |k: &str, v: OsString| {
        env.insert(k.to_string(), v);
    };
    let pkg = input.package;

    set("OUT_DIR", input.out_dir.into());
    set("HOST", input.host_triple.into());

    // RUSTC must be the real binary, not the rustup proxy: the script
    // runs in the package directory, where the proxy could pick a
    // different toolchain from a `rust-toolchain.toml` than the one
    // quon builds with.
    let exe = std::env::consts::EXE_SUFFIX;
    let bin = input.toolchain_sysroot.join("bin");
    set("RUSTC", bin.join(format!("rustc{exe}")).into());
    set("RUSTDOC", bin.join(format!("rustdoc{exe}")).into());
    set("CARGO", input.cargo.into());

    let (opt_level, debug_info) = match input.platform {
        ScriptPlatform::Host => (HOST_OPT_LEVEL, HOST_DEBUG_INFO),
        ScriptPlatform::Cross {
            opt_level,
            debug_info,
            ..
        } => (opt_level, debug_info),
    };
    let profile = if opt_level == quon_model::OptLevel::Zero {
        "debug"
    } else {
        "release"
    };
    set("PROFILE", profile.into());
    set("OPT_LEVEL", opt_level.as_str().into());
    set("DEBUG", debug_info.to_string().into());
    set("NUM_JOBS", input.num_jobs.to_string().into());

    // TARGET and the encoded flags depend on the platform. For a custom
    // JSON target, Cargo sets TARGET to the file stem; rustc resolves
    // the stem through RUST_TARGET_PATH, and reading a JSON spec needs
    // `-Zunstable-options`.
    let mut rustflags: Vec<OsString> = input.target_rustflags.iter().map(OsString::from).collect();
    match input.platform {
        ScriptPlatform::Host => set("TARGET", input.host_triple.into()),
        ScriptPlatform::Cross {
            target,
            quon_sysroot,
            ..
        } => {
            match target {
                ScriptTarget::Builtin(triple) => set("TARGET", triple.into()),
                ScriptTarget::Custom(spec) => {
                    let stem = spec.file_stem().unwrap_or(spec.as_os_str());
                    set("TARGET", stem.into());
                    let dir = spec.parent().unwrap_or(Path::new(""));
                    set("RUST_TARGET_PATH", dir.into());
                }
            }
            // Probe-style scripts run `$RUSTC --target $TARGET
            // $CARGO_ENCODED_RUSTFLAGS` on a test file. A bare-metal
            // target has no installed std, so without quon's sysroot
            // the probe fails and the crate falls back to its most
            // conservative cfgs. With it, the probe sees the same
            // `core` the crate links against.
            let mut sysroot = OsString::from("--sysroot=");
            sysroot.push(quon_sysroot);
            rustflags.push(sysroot);
            if matches!(target, ScriptTarget::Custom(_)) {
                rustflags.push("-Zunstable-options".into());
            }
        }
    }
    set("CARGO_ENCODED_RUSTFLAGS", join_os(&rustflags, "\x1f"));

    set("CARGO_MANIFEST_DIR", pkg.manifest_dir.as_os_str().into());
    set("CARGO_MANIFEST_PATH", pkg.manifest_path.as_os_str().into());
    if let Some(links) = &pkg.links {
        set("CARGO_MANIFEST_LINKS", links.into());
    }

    let (major, minor, patch, pre) = split_version(&pkg.version);
    let opt = |v: &Option<String>| OsString::from(v.as_deref().unwrap_or(""));
    set("CARGO_PKG_NAME", pkg.name.as_str().into());
    set("CARGO_PKG_VERSION", pkg.version.as_str().into());
    set("CARGO_PKG_VERSION_MAJOR", major.into());
    set("CARGO_PKG_VERSION_MINOR", minor.into());
    set("CARGO_PKG_VERSION_PATCH", patch.into());
    set("CARGO_PKG_VERSION_PRE", pre.into());
    set("CARGO_PKG_AUTHORS", pkg.authors.join(":").into());
    set("CARGO_PKG_DESCRIPTION", opt(&pkg.description));
    set("CARGO_PKG_HOMEPAGE", opt(&pkg.homepage));
    set("CARGO_PKG_REPOSITORY", opt(&pkg.repository));
    set("CARGO_PKG_LICENSE", opt(&pkg.license));
    set("CARGO_PKG_LICENSE_FILE", opt(&pkg.license_file));
    set("CARGO_PKG_RUST_VERSION", opt(&pkg.rust_version));
    set("CARGO_PKG_README", opt(&pkg.readme));

    for feature in input.features {
        set(&format!("CARGO_FEATURE_{}", env_name(feature)), "1".into());
    }

    for (name, value) in cfg_vars(input.target_cfgs) {
        set(&format!("CARGO_CFG_{}", env_name(&name)), value.into());
    }

    for (links, pairs) in input.dep_metadata {
        for (key, value) in pairs {
            let name = format!("DEP_{}_{}", env_name(links), env_name(key));
            set(&name, value.into());
        }
    }

    env
}

/// Group `--print cfg` output into one value per name. A name with
/// several values (`target_feature`, `target_has_atomic`) gets them
/// joined with commas in rustc's order. A name without a value
/// (`unix`, `debug_assertions`) maps to the empty string. `feature` is
/// skipped: features reach scripts as `CARGO_FEATURE_*`.
fn cfg_vars(cfgs: &[(String, Option<String>)]) -> BTreeMap<String, String> {
    let mut grouped: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (name, value) in cfgs {
        if name == "feature" {
            continue;
        }
        let values = grouped.entry(name.clone()).or_default();
        if let Some(v) = value {
            values.push(v);
        }
    }
    grouped
        .into_iter()
        .map(|(name, values)| (name, values.join(",")))
        .collect()
}

/// `my-feature` → `MY_FEATURE`, the form Cargo uses in variable names.
fn env_name(s: &str) -> String {
    s.to_uppercase().replace('-', "_")
}

/// Split a semver string into major, minor, patch and pre-release.
/// Build metadata (`+...`) is dropped, as in Cargo. Missing parts are
/// empty; cargo has already validated the version, so this does not.
fn split_version(version: &str) -> (&str, &str, &str, &str) {
    let core_and_pre = version.split('+').next().unwrap_or("");
    let (core, pre) = core_and_pre.split_once('-').unwrap_or((core_and_pre, ""));
    let mut parts = core.splitn(3, '.');
    let mut next = || parts.next().unwrap_or("");
    (next(), next(), next(), pre)
}

fn join_os(parts: &[OsString], sep: &str) -> OsString {
    let mut out = OsString::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push(sep);
        }
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package() -> PackageInfo {
        PackageInfo {
            name: "probe-sys".into(),
            version: "1.2.3-beta.1+build.5".into(),
            authors: vec!["A <a@x>".into(), "B".into()],
            description: Some("probes".into()),
            homepage: None,
            repository: Some("https://example.org/probe".into()),
            license: Some("MIT".into()),
            license_file: None,
            rust_version: Some("1.77".into()),
            readme: None,
            links: Some("probe".into()),
            manifest_dir: "/vendor/probe-sys-1.2.3".into(),
            manifest_path: "/vendor/probe-sys-1.2.3/Cargo.toml".into(),
        }
    }

    fn cfgs() -> Vec<(String, Option<String>)> {
        [
            ("debug_assertions", None),
            ("feature", Some("ignored")),
            ("target_arch", Some("x86_64")),
            ("target_feature", Some("fxsr")),
            ("target_feature", Some("sse")),
            ("target_has_atomic", Some("64")),
            ("target_has_atomic", Some("ptr")),
            ("target_os", Some("none")),
        ]
        .into_iter()
        .map(|(n, v)| (n.to_string(), v.map(str::to_string)))
        .collect()
    }

    /// Render the env as sorted `KEY=VALUE` lines for golden comparison.
    fn render(env: &BTreeMap<String, OsString>) -> String {
        env.iter()
            .map(|(k, v)| format!("{k}={}\n", v.to_string_lossy()))
            .collect()
    }

    fn input<'a>(
        pkg: &'a PackageInfo,
        platform: ScriptPlatform<'a>,
        cfgs: &'a [(String, Option<String>)],
        features: &'a [String],
        flags: &'a [String],
        deps: &'a [(String, Vec<(String, String)>)],
    ) -> ScriptEnvInput<'a> {
        ScriptEnvInput {
            package: pkg,
            features,
            platform,
            out_dir: Path::new("/build/out"),
            host_triple: "aarch64-apple-darwin",
            toolchain_sysroot: Path::new("/toolchain"),
            cargo: Path::new("/bin/cargo"),
            num_jobs: 8,
            target_cfgs: cfgs,
            target_rustflags: flags,
            dep_metadata: deps,
        }
    }

    #[test]
    fn cross_builtin_target_golden() {
        let pkg = package();
        let cfgs = cfgs();
        let features = vec!["default".to_string(), "with-alloc".to_string()];
        let flags = vec!["-Ccode-model=kernel".to_string()];
        let deps = vec![(
            "z-lib".to_string(),
            vec![("include-dir".to_string(), "/z/inc".to_string())],
        )];
        let platform = ScriptPlatform::Cross {
            target: ScriptTarget::Builtin("x86_64-unknown-none"),
            quon_sysroot: Path::new("/build/sysroot/kernel"),
            opt_level: quon_model::OptLevel::Two,
            debug_info: true,
        };
        let env = script_env(&input(&pkg, platform, &cfgs, &features, &flags, &deps));
        let exe = std::env::consts::EXE_SUFFIX;
        let expected = format!(
            "\
CARGO=/bin/cargo
CARGO_CFG_DEBUG_ASSERTIONS=
CARGO_CFG_TARGET_ARCH=x86_64
CARGO_CFG_TARGET_FEATURE=fxsr,sse
CARGO_CFG_TARGET_HAS_ATOMIC=64,ptr
CARGO_CFG_TARGET_OS=none
CARGO_ENCODED_RUSTFLAGS=-Ccode-model=kernel\x1f--sysroot=/build/sysroot/kernel
CARGO_FEATURE_DEFAULT=1
CARGO_FEATURE_WITH_ALLOC=1
CARGO_MANIFEST_DIR=/vendor/probe-sys-1.2.3
CARGO_MANIFEST_LINKS=probe
CARGO_MANIFEST_PATH=/vendor/probe-sys-1.2.3/Cargo.toml
CARGO_PKG_AUTHORS=A <a@x>:B
CARGO_PKG_DESCRIPTION=probes
CARGO_PKG_HOMEPAGE=
CARGO_PKG_LICENSE=MIT
CARGO_PKG_LICENSE_FILE=
CARGO_PKG_NAME=probe-sys
CARGO_PKG_README=
CARGO_PKG_REPOSITORY=https://example.org/probe
CARGO_PKG_RUST_VERSION=1.77
CARGO_PKG_VERSION=1.2.3-beta.1+build.5
CARGO_PKG_VERSION_MAJOR=1
CARGO_PKG_VERSION_MINOR=2
CARGO_PKG_VERSION_PATCH=3
CARGO_PKG_VERSION_PRE=beta.1
DEBUG=true
DEP_Z_LIB_INCLUDE_DIR=/z/inc
HOST=aarch64-apple-darwin
NUM_JOBS=8
OPT_LEVEL=2
OUT_DIR=/build/out
PROFILE=release
RUSTC=/toolchain/bin/rustc{exe}
RUSTDOC=/toolchain/bin/rustdoc{exe}
TARGET=x86_64-unknown-none
"
        );
        assert_eq!(render(&env), expected);
    }

    #[test]
    fn host_unit_uses_host_triple_and_fixed_profile() {
        let mut pkg = package();
        pkg.links = None;
        let host_cfgs = vec![("unix".to_string(), None)];
        let env = script_env(&input(
            &pkg,
            ScriptPlatform::Host,
            &host_cfgs,
            &[],
            &[],
            &[],
        ));
        let get = |k: &str| env.get(k).map(|v| v.to_string_lossy().into_owned());
        assert_eq!(get("TARGET").as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(get("PROFILE").as_deref(), Some("debug"));
        assert_eq!(get("OPT_LEVEL").as_deref(), Some("0"));
        assert_eq!(get("DEBUG").as_deref(), Some("false"));
        assert_eq!(get("CARGO_CFG_UNIX").as_deref(), Some(""));
        // No quon sysroot for host probes: they use the toolchain's std.
        assert_eq!(get("CARGO_ENCODED_RUSTFLAGS").as_deref(), Some(""));
        assert!(get("RUST_TARGET_PATH").is_none());
        assert!(get("CARGO_MANIFEST_LINKS").is_none());
    }

    #[test]
    fn custom_json_target_uses_stem_and_target_path() {
        let pkg = package();
        let platform = ScriptPlatform::Cross {
            target: ScriptTarget::Custom(Path::new("/proj/targets/x86_64-hadron.json")),
            quon_sysroot: Path::new("/build/sysroot/hadron"),
            opt_level: quon_model::OptLevel::Zero,
            debug_info: true,
        };
        let env = script_env(&input(&pkg, platform, &[], &[], &[], &[]));
        let get = |k: &str| env.get(k).map(|v| v.to_string_lossy().into_owned());
        assert_eq!(get("TARGET").as_deref(), Some("x86_64-hadron"));
        assert_eq!(get("RUST_TARGET_PATH").as_deref(), Some("/proj/targets"));
        assert_eq!(
            get("CARGO_ENCODED_RUSTFLAGS").as_deref(),
            Some("--sysroot=/build/sysroot/hadron\x1f-Zunstable-options")
        );
        assert_eq!(get("PROFILE").as_deref(), Some("debug"));
    }

    #[test]
    fn inherited_cargo_dep_and_wrapper_vars_are_removed() {
        let inherited = [
            "CARGO_PKG_NAME",
            "CARGO_MAKEFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "DEP_OPENSSL_ROOT",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
            "PATH",
            "HOME",
            "CC_x86_64_unknown_none",
            "CARGO",
            "RUSTC",
        ];
        let removed: Vec<&str> = inherited
            .into_iter()
            .filter(|n| should_remove_inherited(OsStr::new(n)))
            .collect();
        assert_eq!(
            removed,
            vec![
                "CARGO_PKG_NAME",
                "CARGO_MAKEFLAGS",
                "CARGO_ENCODED_RUSTFLAGS",
                "DEP_OPENSSL_ROOT",
                "RUSTC_WRAPPER",
                "RUSTC_WORKSPACE_WRAPPER",
                "RUSTFLAGS",
            ]
        );
    }

    #[test]
    fn version_split_handles_missing_parts() {
        assert_eq!(split_version("1.2.3"), ("1", "2", "3", ""));
        assert_eq!(split_version("0.8.0-rc.1"), ("0", "8", "0", "rc.1"));
        assert_eq!(split_version("1.0.0+meta-data"), ("1", "0", "0", ""));
    }
}
