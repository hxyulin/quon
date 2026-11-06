//! [`Quon`], the value a `quon.rs` fills in, and its lowering to the
//! [`RawBuildModel`] that `quon-core` reads.

use crate::artifact::{Artifact, ArtifactKind, Source};
use crate::error::{ConfigError, Result};
use crate::files::{BuildStep, Download, Esp, Image};
use crate::krate::{CrateDecl, Dependency};
use crate::opt::Preset;
use crate::profile::{Profile, ProfileRef};
use crate::runner::Runner;
use crate::target::{HOST, Target, TargetKind};
use crate::tests::{BootTest, HostTests};
use quon_model::script_protocol::{REQUEST_PREFIX, ScriptReply, ScriptRequest};
use quon_model::{
    ConfigOptionDef, DownloadDef, ExternalDepDef, ImageDef, ImageEntry, ImageSource, PartitionDef,
    PresetDef, ProjectDef, RawBootTestDef, RawBuildModel, RawBuildStepDef, RawBuildStepInput,
    RawCargoWorkspace, RawCrateDef, RawDepDef, RawEspDef, RawEspEntry, RawEspSource, RawGroupDef,
    RawManifestRoot, RawProfileDef, RawRunDef, RawRunnerDef, RawSource, RawWrite, RunnerKind,
    TargetDef, TargetSpec, WorkspaceMember,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Something a `quon.rs` declares with [`Quon::add`]: a package,
/// runner, profile, ESP, config option and so on. `declare` records
/// it and returns the handle other declarations refer to it by.
///
/// Crates outside Quon can implement it to offer their own typed
/// declarations, built from the ones here:
///
/// ```
/// use quon_config::prelude::*;
///
/// /// A probe-rs runner for one chip.
/// struct ProbeRs { chip: String }
///
/// impl Declare for ProbeRs {
///     type Handle = Runner;
///     fn declare(self, q: &mut Quon) -> Runner {
///         q.add(Plugin::new("probe-rs").setting("chip", self.chip))
///     }
/// }
///
/// let mut q = Quon::new();
/// let board = q.add(ProbeRs { chip: "nRF52840_xxAA".into() });
/// # let _ = board;
/// ```
pub trait Declare {
    type Handle;
    fn declare(self, q: &mut Quon) -> Self::Handle;
}

/// The project a `quon.rs` describes. Quon creates one, passes it to
/// the script's `config` function, and reads it back once `config`
/// returns.
///
/// Two kinds of method fill it in. [`add`](Self::add) declares a named
/// item and returns a handle to it. The others set what a project has
/// one of: its name, Cargo workspace, default target and run entry.
///
/// ```
/// use quon_config::prelude::*;
///
/// pub fn config(q: &mut Quon) -> Result<()> {
///     q.project("my-kernel", "0.1.0");
///     q.cargo_workspace(".");
///     q.default_target(&Target::builtin("x86_64-unknown-none"));
///
///     let kernel = q.add(Package::new("kernel").linker_script("crates/kernel/kernel.ld"));
///     let qemu = q.add(Qemu::new("qemu-system-x86_64").machine("q35").serial(Serial::Stdio));
///     q.run(&kernel).runner(&qemu);
///     Ok(())
/// }
/// # let mut q = Quon::new();
/// # config(&mut q).unwrap();
/// # q.into_raw().unwrap();
/// ```
#[derive(Debug, Default)]
pub struct Quon {
    project: Option<ProjectDef>,
    default_profile: Option<String>,
    default_target: Option<Target>,
    /// Targets declared only so `quon build --target` can pick them.
    targets: Vec<Target>,
    workspace: Option<CargoWorkspace>,
    runs: Vec<Run>,
    host_tests: HostTests,
    /// `q.env(..)`: variables of every rustc and build-script run.
    env: BTreeMap<String, String>,
    kconfig: Vec<PathBuf>,
    crates: Vec<CrateDecl>,
    deps: Vec<Dependency>,
    profiles: Vec<Profile>,
    runners: Vec<RawRunnerDef>,
    downloads: Vec<Download>,
    esps: Vec<Esp>,
    images: Vec<Image>,
    steps: Vec<BuildStep>,
    options: Vec<ConfigOptionDef>,
    presets: Vec<Preset>,
    boot_tests: Vec<BootTest>,
    /// `(namespace, name)` of every named declaration, to report
    /// repeats. Crates and build steps share the `artifact` namespace,
    /// since both are looked up by name as build inputs.
    names: BTreeSet<(&'static str, String)>,
    errors: Vec<ConfigError>,
    /// Whether a quon process answers questions on stdin, as it does
    /// for the `main` it generates around `quon.rs`.
    connected: bool,
}

/// What `quon run` starts, from [`Quon::run`].
#[derive(Debug)]
pub struct Run {
    artifact: Artifact,
    name: Option<String>,
    runner: Option<String>,
    meta: BTreeMap<String, String>,
}

impl Run {
    /// Name the entry, so `quon run <name>` and `quon build <name>`
    /// can pick it. A project with more than one entry names each.
    pub fn name(&mut self, name: impl Into<String>) -> &mut Self {
        self.name = Some(name.into());
        self
    }

    /// Start the binary with `runner`. Without one, `quon run` runs a
    /// host binary directly.
    pub fn runner(&mut self, runner: &Runner) -> &mut Self {
        self.runner = Some(runner.name.clone());
        self
    }

    /// Attach `key = value` to the entry, for tools that read the
    /// project through `quon::Project`. Quon itself ignores it, so
    /// changing it rebuilds nothing. Setting a key again replaces its
    /// value.
    pub fn meta(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.meta.insert(key.into(), value.into());
        self
    }
}

/// The Cargo workspace, from [`Quon::cargo_workspace`].
#[derive(Debug)]
pub struct CargoWorkspace {
    raw: RawCargoWorkspace,
    connected: bool,
    /// From [`members`](Self::members); [`Quon::finish`] reports them.
    errors: Vec<ConfigError>,
}

impl CargoWorkspace {
    /// The workspace's members, sorted by name, so `quon.rs` can
    /// declare an item per package instead of listing them:
    ///
    /// ```no_run
    /// # use quon_config::prelude::*;
    /// # let mut q = Quon::new();
    /// for member in q.cargo_workspace(".").members() {
    ///     if member.path.starts_with("drivers/") {
    ///         q.add(Package::new(&member.name));
    ///     }
    /// }
    /// ```
    ///
    /// quon reads the manifests while the script runs and reruns the
    /// script on every command, so a new or edited `Cargo.toml` shows
    /// up on the next command without recompiling `quon.rs`. Outside
    /// quon, as in a unit test with [`Quon::new`], there is nobody to
    /// ask: the list is empty and Quon reports an error.
    pub fn members(&mut self) -> Vec<WorkspaceMember> {
        let dir = self.raw.dir.clone();
        let fail = |errors: &mut Vec<ConfigError>, reason: &str| {
            errors.push(ConfigError::new(format!(
                "cargo_workspace(\"{dir}\").members(): {reason}"
            )));
            Vec::new()
        };
        if !self.connected {
            return fail(&mut self.errors, "needs quon to run quon.rs");
        }
        match ask(&ScriptRequest::Members { dir: dir.clone() }) {
            Ok(ScriptReply::Members(members)) => members,
            Ok(ScriptReply::Error(reason)) | Err(reason) => fail(&mut self.errors, &reason),
        }
    }

    /// Read registry sources from `dir`, filled by `quon vendor`,
    /// instead of Cargo's registry cache. A build then runs no Cargo
    /// command, and fails if `Cargo.lock` changed since `quon vendor`.
    pub fn vendored(&mut self, dir: impl Into<String>) -> &mut Self {
        self.raw.vendored = Some(dir.into());
        self
    }
}

/// Ask the quon process that started the script, and wait for its
/// answer. See [`quon_model::script_protocol`].
fn ask(request: &ScriptRequest) -> std::result::Result<ScriptReply, String> {
    use std::io::{BufRead, Write};
    let json = serde_json::to_string(request).map_err(|e| e.to_string())?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{REQUEST_PREFIX}{json}")
        .and_then(|()| stdout.flush())
        .map_err(|e| format!("asking quon: {e}"))?;
    drop(stdout);
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("reading quon's answer: {e}"))?;
    if line.is_empty() {
        return Err("quon closed stdin without answering".into());
    }
    serde_json::from_str(&line).map_err(|e| format!("quon's answer is not valid JSON: {e}"))
}

impl Quon {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A `Quon` whose questions, such as
    /// [`CargoWorkspace::members`], the quon process on the other end
    /// of stdin and stdout answers. Only the `main` Quon generates
    /// around `quon.rs` may use it.
    #[doc(hidden)]
    #[must_use]
    pub fn connected() -> Self {
        Self {
            connected: true,
            ..Self::default()
        }
    }

    /// Declare `item` and return its handle.
    pub fn add<D: Declare>(&mut self, item: D) -> D::Handle {
        item.declare(self)
    }

    /// Report an error. The script still runs to the end, so it reports
    /// every error at once, and then Quon stops. For [`Declare`]
    /// implementations that check their input.
    pub fn error(&mut self, message: impl Into<String>) {
        self.errors.push(ConfigError::new(message));
    }

    /// Name the project. The generated config crate is
    /// `<name>_config`.
    pub fn project(&mut self, name: impl Into<String>, version: impl Into<String>) {
        self.project = Some(ProjectDef {
            name: name.into(),
            version: version.into(),
            ..ProjectDef::default()
        });
    }

    /// The profile Quon uses without `-p` or `-r`. Defaults to `dev`.
    pub fn default_profile(&mut self, profile: &ProfileRef) {
        self.default_profile = Some(profile.name.clone());
    }

    /// The target crates build for when they don't name one. Without
    /// it they build for the host.
    pub fn default_target(&mut self, target: &Target) {
        self.default_target = Some(target.clone());
    }

    /// Make `target` available to `quon build --target <name>` even
    /// when no crate names it. Targets that crates use need no
    /// declaration.
    pub fn target(&mut self, target: &Target) {
        self.targets.push(target.clone());
    }

    /// Read workspace packages from the Cargo workspace in `dir`,
    /// relative to the project root. Quon reads the manifests on every
    /// run, so editing a `Cargo.toml` does not recompile `quon.rs`.
    pub fn cargo_workspace(&mut self, dir: impl Into<String>) -> &mut CargoWorkspace {
        let dir = dir.into();
        if let Some(prev) = &self.workspace {
            self.errors.push(ConfigError::new(format!(
                "cargo_workspace is declared more than once (\"{}\" and \"{dir}\")",
                prev.raw.dir
            )));
        }
        self.workspace.insert(CargoWorkspace {
            raw: RawCargoWorkspace {
                dir,
                vendored: None,
            },
            connected: self.connected,
            errors: Vec::new(),
        })
    }

    /// Set `key` to `value` in the environment of every rustc run and
    /// every build script, as Cargo's `[env]` table does. Use it for a
    /// variable a crate reads while it compiles, such as `DEFMT_LOG`,
    /// which defmt's macros read to decide which log levels to keep.
    ///
    /// The value is part of every crate's command, so changing it
    /// rebuilds every crate, the only safe choice when a proc macro
    /// reads it. A variable Quon itself sets for a crate, such as
    /// `CARGO_PKG_NAME` or `OUT_DIR`, keeps Quon's value.
    ///
    /// ```
    /// # use quon_config::prelude::*;
    /// # let mut q = Quon::new();
    /// q.env("DEFMT_LOG", "info");
    /// ```
    pub fn env(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        let key = key.into();
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            self.errors.push(ConfigError::new(format!(
                "q.env: \"{key}\" is not a variable name (it must be non-empty, without '=' or NUL)"
            )));
            return self;
        }
        let value = value.into();
        if let Some(prev) = self.env.get(&key)
            && *prev != value
        {
            self.errors.push(ConfigError::new(format!(
                "q.env: {key} is set twice (\"{prev}\" and \"{value}\")"
            )));
            return self;
        }
        self.env.insert(key, value);
        self
    }

    /// The workspace packages whose tests `quon test` runs on the host.
    pub fn host_tests(&mut self) -> &mut HostTests {
        &mut self.host_tests
    }

    /// Make `artifact` what `quon run` starts and boot tests boot.
    /// Profiles build for its target.
    ///
    /// Call it once per binary to give the project several entries,
    /// each with a [`name`](Run::name). A command then builds, runs or
    /// tests one entry at a time, and resolves Cargo features for that
    /// entry's packages alone, as `cargo build -p` does. Two entries
    /// can therefore enable features of one dependency that exclude
    /// each other.
    pub fn run(&mut self, artifact: &Artifact) -> &mut Run {
        self.runs.push(Run {
            artifact: artifact.clone(),
            name: None,
            runner: None,
            meta: BTreeMap::new(),
        });
        self.runs.last_mut().expect("just pushed")
    }

    /// A handle to the profile `name`: one added with `q.add`, or the
    /// built-in `dev` or `release`. Quon reports an unknown name when
    /// it checks the model.
    #[must_use]
    pub fn profile(&self, name: &str) -> ProfileRef {
        ProfileRef { name: name.into() }
    }

    /// Read config options and presets from a `.kconfig` file, relative
    /// to the project root, after the script runs.
    pub fn include_kconfig(&mut self, path: impl Into<PathBuf>) {
        self.kconfig.push(path.into());
    }

    /// Lower the project into the model `quon-core` reads. Fails with
    /// every error the script reported, one per line.
    pub fn into_raw(self) -> Result<RawBuildModel> {
        let (raw, errors) = self.finish();
        if errors.is_empty() {
            Ok(raw)
        } else {
            let message = errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            Err(ConfigError::new(message))
        }
    }

    /// Print the model as JSON on stdout, for `quon-core` to read. The
    /// `main` Quon generates around `quon.rs` calls this. Errors go to
    /// stderr, and the first one is returned so the process fails.
    pub fn emit(self) -> Result<()> {
        let (raw, errors) = self.finish();
        if let Some(first) = errors.first() {
            for e in &errors {
                eprintln!("{e}");
            }
            return Err(first.clone());
        }
        serde_json::to_writer_pretty(std::io::stdout().lock(), &raw)
            .map_err(|e| ConfigError::new(format!("emit: {e}")))?;
        println!();
        Ok(())
    }

    // -- recording, called by the `Declare` impls -----------------------

    /// Claim `name` in `namespace`. False, with an error, for a repeat.
    fn claim(&mut self, namespace: &'static str, name: &str) -> bool {
        let fresh = self.names.insert((namespace, name.to_string()));
        if !fresh {
            self.errors.push(ConfigError::new(format!(
                "{namespace} '{name}' is declared more than once"
            )));
        }
        fresh
    }

    pub(crate) fn add_crate(&mut self, decl: CrateDecl) -> Artifact {
        let a = Artifact::new(ArtifactKind::Crate, &decl.name);
        if self.claim("artifact", &decl.name) {
            self.crates.push(decl);
        }
        a
    }

    pub(crate) fn add_dependency(&mut self, dep: Dependency) -> Artifact {
        let a = Artifact::new(ArtifactKind::Dependency, &dep.name);
        if self.claim("dependency", &dep.name) {
            self.deps.push(dep);
        }
        a
    }

    pub(crate) fn add_profile(&mut self, profile: Profile) -> ProfileRef {
        let r = ProfileRef {
            name: profile.name.clone(),
        };
        if self.claim("profile", &profile.name) {
            self.profiles.push(profile);
        }
        r
    }

    /// Runners are named after their kind (`qemu`, `command`, or the
    /// plugin), numbered from the second: nothing outside the model
    /// refers to them by name.
    pub(crate) fn add_runner(&mut self, base: &str, kind: RunnerKind) -> Runner {
        let mut name = base.to_string();
        let mut n = 1;
        while self.runners.iter().any(|r| r.name == name) {
            n += 1;
            name = format!("{base}-{n}");
        }
        self.runners.push(RawRunnerDef {
            name: name.clone(),
            kind: Some(kind),
            span: None,
        });
        Runner { name }
    }

    pub(crate) fn add_download(&mut self, d: Download) -> Artifact {
        let a = Artifact::new(ArtifactKind::Download, &d.name);
        if self.claim("download", &d.name) {
            self.downloads.push(d);
        }
        a
    }

    pub(crate) fn add_esp(&mut self, esp: Esp) -> Artifact {
        let a = Artifact::new(ArtifactKind::Esp, &esp.name);
        if self.claim("esp", &esp.name) {
            self.esps.push(esp);
        }
        a
    }

    pub(crate) fn add_image(&mut self, image: Image) -> Artifact {
        let a = Artifact::new(ArtifactKind::Image, &image.name);
        if self.claim("image", &image.name) {
            self.images.push(image);
        }
        a
    }

    pub(crate) fn add_build_step(&mut self, step: BuildStep) -> Artifact {
        let a = Artifact::new(ArtifactKind::BuildStep, &step.name);
        if self.claim("artifact", &step.name) {
            self.steps.push(step);
        }
        a
    }

    pub(crate) fn add_option(&mut self, def: ConfigOptionDef) {
        if self.claim("config option", &def.name) {
            self.options.push(def);
        }
    }

    pub(crate) fn add_preset(&mut self, preset: Preset) -> crate::opt::PresetRef {
        let r = crate::opt::PresetRef {
            name: preset.name.clone(),
        };
        if self.claim("preset", &preset.name) {
            self.presets.push(preset);
        }
        r
    }

    pub(crate) fn add_boot_test(&mut self, test: BootTest) {
        if self.claim("boot test", &test.name) {
            self.boot_tests.push(test);
        }
    }

    // -- lowering --------------------------------------------------------

    /// Lower everything into a [`RawBuildModel`], with the errors the
    /// script and the lowering found. Lowering happens here rather than
    /// in `add` because defaults (the default target, the run entry)
    /// can be set after the declarations that use them.
    pub fn finish(mut self) -> (RawBuildModel, Vec<ConfigError>) {
        let mut l = Lowering {
            raw: RawBuildModel::default(),
            errors: std::mem::take(&mut self.errors),
        };
        if let Some(ws) = &mut self.workspace {
            l.errors.append(&mut ws.errors);
        }

        let default_target = self.default_target.clone().unwrap_or_else(Target::host);
        for t in self.default_target.iter().chain(&self.targets) {
            l.target(t);
        }

        let host = Target::host();
        for decl in &self.crates {
            // A proc macro runs inside rustc, so it builds for the host
            // unless it names a target.
            let fallback = if decl.crate_type == quon_model::CrateType::ProcMacro {
                &host
            } else {
                &default_target
            };
            let target = decl.target.as_ref().unwrap_or(fallback);
            l.lower_crate(decl, target, self.workspace.is_some());
        }
        for dep in &self.deps {
            l.lower_dependency(dep);
        }

        let runs = self.lower_runs(&default_target, &mut l);
        // A single entry fills the profiles here. With several, the
        // profiles wait for quon-core to pick one, so they keep only
        // what they set themselves.
        let single = match runs.as_slice() {
            [only] => Some(only),
            _ => None,
        };
        let profile_target = if runs.len() > 1 {
            None
        } else {
            self.profile_target(single, &default_target, &mut l)
        };
        for name in ["dev", "release"] {
            if !self.profiles.iter().any(|p| p.name == name) {
                self.profiles.push(if name == "dev" {
                    Profile::dev()
                } else {
                    Profile::release()
                });
            }
        }
        for p in &self.profiles {
            l.lower_profile(p, profile_target.as_deref(), single);
        }

        for r in self.runners {
            l.raw.runners.insert(r.name.clone(), r);
        }
        for d in &self.downloads {
            l.lower_download(d);
        }
        for e in &self.esps {
            l.lower_esp(e);
        }
        for i in &self.images {
            l.lower_image(i);
        }
        for s in &self.steps {
            l.lower_build_step(s);
        }
        for o in self.options {
            l.raw.config_options.insert(o.name.clone(), o);
        }
        for p in self.presets {
            l.raw.presets.insert(
                p.name.clone(),
                PresetDef {
                    name: p.name,
                    inherits: p.inherits,
                    help: p.help,
                    overrides: p.values.into_iter().collect(),
                },
            );
        }
        for t in self.boot_tests {
            l.raw.boot_tests.insert(
                t.name.clone(),
                RawBootTestDef {
                    name: t.name,
                    profile: Some(t.profile),
                    span: None,
                },
            );
        }

        l.raw.project = self.project.map(|mut p| {
            p.default_profile = Some(self.default_profile.unwrap_or_else(|| "dev".into()));
            p
        });
        l.raw.cargo_workspace = self.workspace.map(|w| w.raw);
        l.raw.host_tests = self.host_tests.packages;
        l.raw.host_test_features = self.host_tests.features;
        l.raw.host_test_env = self.host_tests.env;
        l.raw.env = self.env;
        l.raw.kconfig_includes = self.kconfig;
        l.raw.runs = runs;
        (l.raw, l.errors)
    }

    /// Check the run entries and lower them in declaration order. An
    /// entry whose artifact is not a crate is reported and left out.
    fn lower_runs(&self, default_target: &Target, l: &mut Lowering) -> Vec<RawRunDef> {
        let several = self.runs.len() > 1;
        let mut names = BTreeSet::new();
        let mut runs = Vec::new();
        for run in &self.runs {
            match &run.name {
                Some(name) => {
                    if !valid_run_name(name) {
                        l.error(format!(
                            "run '{}': the name '{name}' must be non-empty and use only \
                             letters, digits, '-', '_' and '.'",
                            run.artifact.name
                        ));
                    } else if !names.insert(name.as_str()) {
                        l.error(format!("run entry '{name}' is declared more than once"));
                    }
                }
                None if several => l.error(format!(
                    "run '{}' has no name; with several run entries, name each with \
                     q.run(..).name(\"<name>\")",
                    run.artifact.name
                )),
                None => {}
            }
            if run.artifact.kind != ArtifactKind::Crate {
                l.error(format!(
                    "run: '{}' is a {}, not a crate",
                    run.artifact.name,
                    run.artifact.kind.describe()
                ));
                continue;
            }
            let target = self
                .crates
                .iter()
                .find(|c| c.name == run.artifact.owner)
                .and_then(|c| c.target.as_ref())
                .unwrap_or(default_target);
            runs.push(RawRunDef {
                name: run.name.clone(),
                binary: run.artifact.name.clone(),
                owner: run.artifact.owner.clone(),
                target: l.target(target),
                runner: run.runner.clone(),
                meta: run.meta.clone(),
            });
        }
        runs
    }

    /// The target profiles build for, by name: the single run entry's,
    /// else the default target, else the one target the project's
    /// crates and dependencies use. `None` reports an error already
    /// pushed, or a run entry that was left out.
    fn profile_target(
        &self,
        single: Option<&RawRunDef>,
        default_target: &Target,
        l: &mut Lowering,
    ) -> Option<String> {
        if let Some(run) = single {
            return Some(run.target.clone());
        }
        if !self.runs.is_empty() {
            // The entry was left out with an error.
            return None;
        }
        if self.default_target.is_some() {
            return Some(default_target.name().to_string());
        }
        let used: BTreeSet<&str> = self
            .crates
            .iter()
            .filter_map(|c| c.target.as_ref())
            .chain(self.deps.iter().map(|d| &d.target))
            .filter(|t| !t.is_host())
            .map(Target::name)
            .collect();
        match used.len() {
            0 => Some(HOST.into()),
            1 => used.first().map(|t| (*t).to_string()),
            _ => {
                l.error(format!(
                    "the project builds for several targets ({}); choose the one profiles \
                     build for with q.default_target(..) or q.run(..)",
                    used.into_iter().collect::<Vec<_>>().join(", ")
                ));
                None
            }
        }
    }
}

/// Whether `name` can name a run entry: it is typed on the command
/// line, so it keeps to characters no shell treats specially.
fn valid_run_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

struct Lowering {
    raw: RawBuildModel,
    errors: Vec<ConfigError>,
}

/// The variable through which a build step learns its output path.
/// Mirrors `quon_core::compile::build_step::RESERVED_OUTPUT_ENV`.
const RESERVED_OUTPUT_ENV: &str = "QUON_OUTPUT_PATH";

impl Lowering {
    fn error(&mut self, message: String) {
        self.errors.push(ConfigError::new(message));
    }

    /// Register `t` in the model and return its name. A name declared
    /// twice with different definitions is an error; the first stays.
    fn target(&mut self, t: &Target) -> String {
        let (spec, builtin) = match &t.kind {
            TargetKind::Host => {
                if t.build_std {
                    self.error(
                        "Target::host() cannot use build_std(): host crates link the \
                         toolchain's standard library"
                            .into(),
                    );
                }
                // Host crates include build scripts and proc macros,
                // which every target shares, so a host flag or name
                // would reach far more than the host target.
                if t.name.is_some() || !t.rustc_flags.is_empty() {
                    self.error(
                        "Target::host() cannot use named() or rustc_flag(): it builds \
                         every target's build scripts and proc macros"
                            .into(),
                    );
                }
                return HOST.into();
            }
            TargetKind::Builtin(triple) => (triple.clone(), true),
            TargetKind::Spec { path, .. } => (path.clone(), false),
        };
        // A custom target always builds its sysroot, so the flag only
        // tells builtin targets apart.
        let build_std = builtin && t.build_std;
        let name = t.name().to_string();
        if name == HOST {
            self.error(format!(
                "target name \"{HOST}\" is reserved for Target::host()"
            ));
        }
        let def = TargetDef {
            name: name.clone(),
            spec: spec.clone(),
            builtin,
            build_std,
            panic_strategy: None,
            rustc_flags: t.rustc_flags.clone(),
            span: None,
        };
        let (h, inserted) = self.raw.targets.insert(name.clone(), def);
        if !inserted
            && let Some(prev) = self.raw.targets.get(h)
            && (prev.spec != spec
                || prev.builtin != builtin
                || prev.build_std != build_std
                || prev.rustc_flags != t.rustc_flags)
        {
            let describe = |spec: &str, builtin: bool, build_std: bool, flags: &[String]| {
                let mut text = match (builtin, build_std) {
                    (true, true) => format!("builtin target \"{spec}\" with build_std()"),
                    (true, false) => format!("builtin target \"{spec}\""),
                    (false, _) => format!("custom spec \"{spec}\""),
                };
                if !flags.is_empty() {
                    text.push_str(&format!(" with rustc flags {flags:?}"));
                }
                text
            };
            let message = format!(
                "target \"{name}\" is defined twice: as {} and as {}",
                describe(&prev.spec, prev.builtin, prev.build_std, &prev.rustc_flags),
                describe(&spec, builtin, build_std, &t.rustc_flags)
            );
            self.error(message);
        }
        name
    }

    /// One crate and its own group. Groups carry the settings a crate
    /// shares with its group in the model (target, edition, config
    /// crate, sysroot); one per crate keeps those per crate.
    fn lower_crate(&mut self, c: &CrateDecl, target: &Target, has_workspace: bool) {
        let target = self.target(target);
        if c.package.is_some() && !has_workspace {
            self.error(format!(
                "package '{}' needs a Cargo workspace: add q.cargo_workspace(\"<dir>\")",
                c.name
            ));
        }

        let mut deps = BTreeMap::new();
        for d in &c.deps {
            if !matches!(d.on.kind, ArtifactKind::Crate | ArtifactKind::Dependency) {
                self.error(format!(
                    "crate '{}' depends on '{}', a {}; a dependency must be a crate",
                    c.name,
                    d.on.name,
                    d.on.kind.describe()
                ));
                continue;
            }
            deps.insert(
                d.extern_name(),
                RawDepDef {
                    crate_name: d.on.name.clone(),
                    features: d.features.clone(),
                    version: d.version.clone(),
                    optional: d.optional,
                    default_features: d.default_features,
                    span: None,
                },
            );
        }

        let mut artifact_env = BTreeMap::new();
        let mut artifact_deps = Vec::new();
        for (var, a) in &c.artifact_env {
            if !a.is_buildable() {
                self.error(format!(
                    "crate '{}' artifact_env(\"{var}\"): '{}' is a {}, not a crate or build step",
                    c.name,
                    a.name,
                    a.kind.describe()
                ));
                continue;
            }
            if var.is_empty() {
                self.error(format!(
                    "crate '{}' artifact_env: the variable name is empty",
                    c.name
                ));
                continue;
            }
            artifact_env.insert(var.clone(), a.name.clone());
            if !artifact_deps.contains(&a.name) {
                artifact_deps.push(a.name.clone());
            }
        }

        self.raw.groups.insert(
            c.name.clone(),
            RawGroupDef {
                name: c.name.clone(),
                target: target.clone(),
                default_edition: c.edition.as_str().into(),
                crates: vec![c.name.clone()],
                config: c.link_config,
                no_sysroot: c.no_sysroot,
                ..RawGroupDef::default()
            },
        );
        self.raw.crates.insert(
            c.name.clone(),
            RawCrateDef {
                name: c.name.clone(),
                path: c.path.clone(),
                edition: c.edition.as_str().into(),
                crate_type: c.crate_type,
                target,
                deps,
                root: c.root.clone(),
                linker_script: c.linker_script.clone(),
                group: c.name.clone(),
                is_project_crate: true,
                cfg_flags: c.cfg_flags.clone(),
                rustc_flags: c.rustc_flags.clone(),
                artifact_deps,
                artifact_env,
                manifest: c.package.as_ref().map(|p| RawManifestRoot {
                    package: c.name.clone(),
                    features: p.features.clone(),
                    default_features: p.default_features,
                }),
                ..RawCrateDef::default()
            },
        );
    }

    fn lower_dependency(&mut self, d: &Dependency) {
        if d.misplaced_git_ref {
            self.error(format!(
                "dependency '{}': .rev, .tag and .branch need a Dependency::git source",
                d.name
            ));
        }
        let target = if d.target.is_host() {
            TargetSpec::Host
        } else {
            let name = self.target(&d.target);
            match self.raw.targets.lookup(&name) {
                Some(h) => TargetSpec::Cross(h),
                None => TargetSpec::Host,
            }
        };
        self.raw.external_deps.insert(
            d.name.clone(),
            ExternalDepDef {
                name: d.name.clone(),
                source: d.source.clone(),
                features: d.features.clone(),
                default_features: d.default_features,
                cfg_flags: d.cfg_flags.clone(),
                rustc_flags: d.rustc_flags.clone(),
                target,
                span: None,
            },
        );
    }

    /// A root profile (one that inherits nothing) gets the target, run
    /// binary and runner; profiles that inherit take them from it, so a
    /// parent's runner override reaches its children.
    fn lower_profile(&mut self, p: &Profile, target: Option<&str>, run: Option<&RawRunDef>) {
        let root = p.inherits.is_none();
        let opt_level = match &p.opt_level {
            Some(Ok(level)) => Some(*level),
            Some(Err(value)) => {
                self.error(format!(
                    "profile '{}': opt_level {value} is not 0, 1, 2, 3, \"s\" or \"z\"",
                    p.name
                ));
                None
            }
            None => None,
        };
        let runner = p
            .runner
            .clone()
            .or_else(|| run.and_then(|r| r.runner.clone()).filter(|_| root));
        self.raw.profiles.insert(
            p.name.clone(),
            RawProfileDef {
                name: p.name.clone(),
                inherits: p.inherits.clone(),
                target: target.filter(|_| root).map(Into::into),
                run_binary: run.filter(|_| root).map(|r| r.binary.clone()),
                runner,
                opt_level,
                debug_info: p.debug_info,
                debug_assertions: p.debug_assertions,
                overflow_checks: p.overflow_checks,
                lto: p.lto.map(|l| l.as_str().into()),
                preset: p.preset.clone(),
                ..RawProfileDef::default()
            },
        );
    }

    fn lower_download(&mut self, d: &Download) {
        if !d.sha256.is_empty()
            && !(d.sha256.len() == 64 && d.sha256.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            self.error(format!(
                "download '{}': '{}' is not a SHA-256 (expected 64 hex digits)",
                d.name, d.sha256
            ));
        }
        self.raw.downloads.insert(
            d.name.clone(),
            DownloadDef {
                name: d.name.clone(),
                url: d.url.clone(),
                sha256: d.sha256.clone(),
                span: None,
            },
        );
    }

    fn lower_esp(&mut self, e: &Esp) {
        let mut entries = Vec::new();
        for (source, dest) in &e.entries {
            if dest.starts_with('/') || dest.starts_with('\\') {
                self.error(format!(
                    "esp '{}': destination '{dest}' must be relative to the ESP root",
                    e.name
                ));
                continue;
            }
            let source = match source {
                Source::Artifact(a) if a.kind == ArtifactKind::Crate => {
                    RawEspSource::Crate(a.name.clone())
                }
                Source::Artifact(a) if a.kind == ArtifactKind::Download => {
                    RawEspSource::Download(a.name.clone())
                }
                Source::File(path) => RawEspSource::File(path.clone()),
                other => {
                    self.error(format!(
                        "esp '{}': {} can't go in an ESP; add a crate, download or file",
                        e.name,
                        describe_source(other)
                    ));
                    continue;
                }
            };
            entries.push(RawEspEntry {
                source,
                dest_path: dest.clone(),
            });
        }
        self.raw.esps.insert(
            e.name.clone(),
            RawEspDef {
                name: e.name.clone(),
                entries,
                span: None,
            },
        );
    }

    /// An image or partition entry. Build steps share the crate
    /// namespace, which is how the model refers to them.
    fn image_entry(&mut self, image: &str, source: &Source, dest: &str) -> Option<ImageEntry> {
        let source = match source {
            Source::Artifact(a) if a.is_buildable() => ImageSource::Crate(a.name.clone()),
            Source::Artifact(a) if a.kind == ArtifactKind::Download => {
                ImageSource::Download(a.name.clone())
            }
            Source::Artifact(a) if a.kind == ArtifactKind::Esp => ImageSource::Esp(a.name.clone()),
            Source::File(path) => ImageSource::File(path.clone()),
            other => {
                self.error(format!(
                    "image '{image}': {} can't be copied into an image",
                    describe_source(other)
                ));
                return None;
            }
        };
        Some(ImageEntry {
            source,
            dest_path: dest.into(),
        })
    }

    fn lower_image(&mut self, i: &Image) {
        let entries = i
            .entries
            .iter()
            .filter_map(|(s, d)| self.image_entry(&i.name, s, d))
            .collect();
        let partitions = i
            .partitions
            .iter()
            .map(|p| PartitionDef {
                name: p.name.clone(),
                start_lba: p.start_lba,
                end_lba: p.end_lba,
                fs_type: p.fs_type.clone(),
                partition_type: p.partition_type,
                bootable: p.bootable,
                entries: p
                    .entries
                    .iter()
                    .filter_map(|(s, d)| self.image_entry(&i.name, s, d))
                    .collect(),
                span: None,
            })
            .collect();
        let mut raw_writes = Vec::new();
        for (lba, source, max_sectors) in &i.raw_writes {
            let source = match source {
                Source::Artifact(a) if a.is_buildable() => RawSource::Crate {
                    name: a.name.clone(),
                },
                Source::File(path) => RawSource::File { path: path.clone() },
                Source::Bytes(bytes) => RawSource::Bytes {
                    bytes: bytes.clone(),
                },
                other => {
                    self.error(format!(
                        "image '{}': raw_write takes a crate, build step, file or bytes, not {}",
                        i.name,
                        describe_source(other)
                    ));
                    continue;
                }
            };
            raw_writes.push(RawWrite {
                lba: *lba,
                source,
                max_sectors: *max_sectors,
                span: None,
            });
        }
        self.raw.images.insert(
            i.name.clone(),
            ImageDef {
                name: i.name.clone(),
                format: i.format,
                size_mb: i.size_mb,
                entries,
                partition_table: i.partition_table,
                partitions,
                raw_writes,
                ..ImageDef::default()
            },
        );
    }

    fn lower_build_step(&mut self, s: &BuildStep) {
        let runner = match &s.tool {
            Some(t) if t.kind == ArtifactKind::Crate => t.name.clone(),
            Some(t) => {
                self.error(format!(
                    "build step '{}': the tool '{}' is a {}, not a crate",
                    s.name,
                    t.name,
                    t.kind.describe()
                ));
                String::new()
            }
            None => {
                self.error(format!(
                    "build step '{}' has no tool; set one with .tool(&artifact)",
                    s.name
                ));
                String::new()
            }
        };
        if s.output.is_empty() || s.output.contains(['/', '\\']) {
            self.error(format!(
                "build step '{}': output '{}' must be a plain file name",
                s.name, s.output
            ));
        }
        for var in s.inputs.keys().chain(s.env.keys()) {
            if var.is_empty() || var == RESERVED_OUTPUT_ENV {
                self.error(format!(
                    "build step '{}': '{var}' can't be a variable name; \
                     {RESERVED_OUTPUT_ENV} is reserved for the output path",
                    s.name
                ));
            }
        }
        let mut inputs = BTreeMap::new();
        for (var, source) in &s.inputs {
            let input = match source {
                Source::Artifact(a) if a.is_buildable() => RawBuildStepInput::Crate {
                    crate_name: a.name.clone(),
                },
                Source::File(path) => RawBuildStepInput::File { path: path.clone() },
                other => {
                    self.error(format!(
                        "build step '{}': input '{var}' is {}; use a crate, build step or file",
                        s.name,
                        describe_source(other)
                    ));
                    continue;
                }
            };
            inputs.insert(var.clone(), input);
        }
        self.raw.build_steps.insert(
            s.name.clone(),
            RawBuildStepDef {
                name: s.name.clone(),
                runner,
                inputs,
                env: s.env.clone(),
                output: s.output.clone(),
                span: None,
            },
        );
    }
}

fn describe_source(s: &Source) -> String {
    match s {
        Source::Artifact(a) => format!("'{}' (a {})", a.name, a.kind.describe()),
        Source::File(path) => format!("the file '{path}'"),
        Source::Bytes(_) => "literal bytes".into(),
    }
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;
    use quon_model::{ConfigValue, Expr, RawBuildModel, RawEspSource, RunnerKind};

    fn lower(f: impl FnOnce(&mut Quon)) -> (RawBuildModel, Vec<String>) {
        let mut q = Quon::new();
        q.project("t", "0.1.0");
        q.cargo_workspace(".");
        f(&mut q);
        let (raw, errors) = q.finish();
        (raw, errors.into_iter().map(|e| e.message).collect())
    }

    fn ok(f: impl FnOnce(&mut Quon)) -> RawBuildModel {
        let (raw, errors) = lower(f);
        assert!(errors.is_empty(), "{errors:?}");
        raw
    }

    fn profile<'a>(raw: &'a RawBuildModel, name: &str) -> &'a quon_model::RawProfileDef {
        raw.profiles
            .get(raw.profiles.lookup(name).expect(name))
            .unwrap()
    }

    #[test]
    fn package_gets_its_own_group_on_the_default_target() {
        let raw = ok(|q| {
            q.default_target(&Target::builtin("x86_64-unknown-none"));
            q.add(Package::new("kernel").linker_script("kernel.ld"));
        });
        let c = raw
            .crates
            .get(raw.crates.lookup("kernel").unwrap())
            .unwrap();
        assert_eq!(c.target, "x86_64-unknown-none");
        assert_eq!(c.group, "kernel");
        assert_eq!(c.manifest.as_ref().unwrap().package, "kernel");
        let g = raw
            .groups
            .get(raw.groups.lookup("kernel").unwrap())
            .unwrap();
        assert_eq!(g.crates, ["kernel"]);
        assert!(raw.targets.lookup("x86_64-unknown-none").is_some());
    }

    #[test]
    fn run_metadata_reaches_the_raw_entry_and_a_repeated_key_keeps_the_last_value() {
        let raw = ok(|q| {
            let fw = q.add(Package::new("fw"));
            q.run(&fw)
                .meta("chip", "STM32F407")
                .meta("bsp", "c-board")
                .meta("chip", "STM32H723");
        });
        let meta: Vec<_> = raw.runs[0]
            .meta
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(meta, [("bsp", "c-board"), ("chip", "STM32H723")]);
    }

    #[test]
    fn dev_and_release_always_exist() {
        let raw = ok(|_| {});
        assert_eq!(profile(&raw, "dev").opt_level, Some(OptLevel::Zero));
        assert_eq!(profile(&raw, "dev").target.as_deref(), Some("host"));
        let release = profile(&raw, "release");
        assert_eq!(release.inherits.as_deref(), Some("dev"));
        assert_eq!(release.opt_level, Some(OptLevel::Three));
        assert_eq!(release.target, None, "inherited from dev");
        let dev = profile(&raw, "dev");
        assert_eq!(
            (dev.debug_assertions, dev.overflow_checks),
            (Some(true), Some(true))
        );
        assert_eq!(
            (release.debug_assertions, release.overflow_checks),
            (Some(false), Some(false))
        );
        assert_eq!(raw.project.unwrap().default_profile.as_deref(), Some("dev"));
    }

    #[test]
    fn a_declared_profile_replaces_the_builtin_one() {
        let raw = ok(|q| {
            q.add(Profile::release().lto(Lto::Thin));
        });
        assert_eq!(profile(&raw, "release").lto.as_deref(), Some("thin"));
        assert_eq!(raw.profiles.len(), 2);
    }

    #[test]
    fn profiles_build_for_the_run_artifacts_target() {
        let raw = ok(|q| {
            let none = Target::builtin("x86_64-unknown-none");
            let uefi = Target::builtin("x86_64-unknown-uefi");
            let kernel = q.add(Package::new("kernel").target(&none));
            let boot = q.add(
                Package::new("bootloader")
                    .target(&uefi)
                    .artifact_env("KERNEL_PATH", &kernel),
            );
            let qemu = q.add(Qemu::new("qemu-system-x86_64"));
            q.run(&boot).runner(&qemu);
        });
        let dev = profile(&raw, "dev");
        assert_eq!(dev.target.as_deref(), Some("x86_64-unknown-uefi"));
        assert_eq!(dev.run_binary.as_deref(), Some("bootloader"));
        assert_eq!(dev.runner.as_deref(), Some("qemu"));
        let boot = raw
            .crates
            .get(raw.crates.lookup("bootloader").unwrap())
            .unwrap();
        assert_eq!(boot.artifact_env["KERNEL_PATH"], "kernel");
        assert_eq!(boot.artifact_deps, ["kernel"]);
    }

    #[test]
    fn a_profile_runner_overrides_the_run_entrys() {
        let raw = ok(|q| {
            let kernel = q.add(Package::new("kernel"));
            let uefi = q.add(Qemu::new("qemu-system-x86_64").boot(BootMode::Uefi));
            let bios = q.add(Qemu::new("qemu-system-x86_64").boot(BootMode::Bios));
            q.run(&kernel).runner(&uefi);
            let dev = q.profile("dev");
            q.add(Profile::new("bios").inherits(&dev).runner(&bios));
        });
        assert_eq!(profile(&raw, "dev").runner.as_deref(), Some("qemu"));
        assert_eq!(profile(&raw, "bios").runner.as_deref(), Some("qemu-2"));
        assert_eq!(profile(&raw, "release").runner, None, "inherited from dev");
    }

    #[test]
    fn several_run_entries_leave_the_profiles_to_quon_core() {
        let raw = ok(|q| {
            let m7 = Target::builtin("thumbv7em-none-eabihf");
            let m0 = Target::builtin("thumbv6m-none-eabi");
            let probe = q.add(Command::new(["probe-rs", "run", "{binary}"]));
            let gimbal = q.add(Package::new("gimbal").target(&m7));
            let tools = q.add(Package::new("tools").target(&m0));
            q.run(&gimbal).name("gimbal").runner(&probe);
            q.run(&tools.bin("flasher")).name("flasher");
        });
        let dev = profile(&raw, "dev");
        assert_eq!(dev.target, None);
        assert_eq!(dev.run_binary, None);
        assert_eq!(dev.runner, None);
        let runs: Vec<_> = raw
            .runs
            .iter()
            .map(|r| {
                (
                    r.name.as_deref(),
                    r.binary.as_str(),
                    r.owner.as_str(),
                    r.target.as_str(),
                    r.runner.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            runs,
            [
                (
                    Some("gimbal"),
                    "gimbal",
                    "gimbal",
                    "thumbv7em-none-eabihf",
                    Some("command")
                ),
                (
                    Some("flasher"),
                    "flasher",
                    "tools",
                    "thumbv6m-none-eabi",
                    None
                ),
            ]
        );
    }

    #[test]
    fn a_single_named_entry_fills_the_profiles() {
        let raw = ok(|q| {
            let kernel = q.add(Package::new("kernel"));
            q.run(&kernel).name("kernel");
        });
        assert_eq!(profile(&raw, "dev").run_binary.as_deref(), Some("kernel"));
        assert_eq!(raw.runs.len(), 1);
    }

    #[test]
    fn several_run_entries_need_unique_valid_names() {
        let (_, errors) = lower(|q| {
            let a = q.add(Package::new("a"));
            let b = q.add(Package::new("b"));
            let c = q.add(Package::new("c"));
            q.run(&a);
            q.run(&b).name("same");
            q.run(&c).name("same");
            q.run(&c).name("has space");
        });
        assert_eq!(
            errors,
            [
                "run 'a' has no name; with several run entries, name each with \
                 q.run(..).name(\"<name>\")",
                "run entry 'same' is declared more than once",
                "run 'c': the name 'has space' must be non-empty and use only letters, \
                 digits, '-', '_' and '.'",
            ]
        );
    }

    #[test]
    fn one_target_in_use_needs_no_default() {
        let raw = ok(|q| {
            let t = Target::spec("x86_64-test", "targets/test.json");
            q.add(Crate::lib("a", "crates/a").target(&t));
        });
        assert_eq!(profile(&raw, "dev").target.as_deref(), Some("x86_64-test"));
    }

    #[test]
    fn several_targets_in_use_need_a_choice() {
        let (_, errors) = lower(|q| {
            q.add(Crate::lib("a", "a").target(&Target::builtin("x")));
            q.add(Crate::lib("b", "b").target(&Target::builtin("y")));
        });
        assert!(errors[0].contains("several targets (x, y)"), "{errors:?}");
    }

    #[test]
    fn repeated_names_are_reported() {
        let (raw, errors) = lower(|q| {
            q.add(Crate::lib("a", "a"));
            q.add(
                BuildStep::new("a", "out")
                    .tool(&Artifact::new(crate::artifact::ArtifactKind::Crate, "a")),
            );
            q.add(Profile::new("p"));
            q.add(Profile::new("p"));
        });
        assert_eq!(
            errors,
            [
                "artifact 'a' is declared more than once",
                "profile 'p' is declared more than once"
            ]
        );
        assert!(raw.build_steps.is_empty());
    }

    #[test]
    fn a_target_name_with_two_definitions_is_reported() {
        let (_, errors) = lower(|q| {
            q.add(Crate::lib("a", "a").target(&Target::builtin("x")));
            q.add(Crate::lib("b", "b").target(&Target::spec("x", "x.json")));
            q.default_target(&Target::builtin("x"));
        });
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("target \"x\" is defined twice"));
    }

    #[test]
    fn named_targets_share_a_triple_carry_their_own_flags_and_a_name_is_defined_once() {
        let m7 = Target::builtin("thumbv7em-none-eabihf")
            .named("cortex-m7")
            .rustc_flag("-Ctarget-cpu=cortex-m7");
        let m4 = Target::builtin("thumbv7em-none-eabihf")
            .named("cortex-m4")
            .rustc_flag("-Ctarget-cpu=cortex-m4");
        let (raw, errors) = lower(|q| {
            q.add(Crate::lib("a", "a").target(&m7));
            q.add(Crate::lib("b", "b").target(&m4));
            q.default_target(&m7);
        });
        assert!(errors.is_empty(), "{errors:?}");
        let targets: Vec<_> = raw
            .targets
            .iter()
            .map(|(_, t)| (t.name.as_str(), t.spec.as_str(), t.rustc_flags.clone()))
            .collect();
        assert_eq!(
            targets,
            [
                (
                    "cortex-m7",
                    "thumbv7em-none-eabihf",
                    vec!["-Ctarget-cpu=cortex-m7".to_owned()]
                ),
                (
                    "cortex-m4",
                    "thumbv7em-none-eabihf",
                    vec!["-Ctarget-cpu=cortex-m4".to_owned()]
                ),
            ]
        );

        let (_, errors) = lower(|q| {
            q.add(Crate::lib("a", "a").target(&m7));
            q.add(Crate::lib("b", "b").target(&m4.clone().named("cortex-m7")));
        });
        assert_eq!(
            errors,
            ["target \"cortex-m7\" is defined twice: as builtin target \
                 \"thumbv7em-none-eabihf\" with rustc flags [\"-Ctarget-cpu=cortex-m7\"] \
                 and as builtin target \"thumbv7em-none-eabihf\" with rustc flags \
                 [\"-Ctarget-cpu=cortex-m4\"]"]
        );

        let (_, errors) = lower(|q| {
            q.add(Crate::lib("a", "a").target(&Target::host().rustc_flag("-Copt-level=3")));
        });
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("Target::host() cannot use named() or rustc_flag()"));
    }

    #[test]
    fn build_std_reaches_the_model_and_must_agree() {
        let (raw, errors) = lower(|q| {
            q.add(Crate::lib("a", "a").target(&Target::builtin("x").build_std()));
            q.add(Crate::lib("b", "b").target(&Target::builtin("y")));
            q.default_target(&Target::builtin("y"));
        });
        assert!(errors.is_empty(), "{errors:?}");
        let def = |name: &str| raw.targets.iter().find(|(_, t)| t.name == name).unwrap().1;
        assert!(def("x").build_std);
        assert!(!def("y").build_std);

        let (_, errors) = lower(|q| {
            q.add(Crate::lib("a", "a").target(&Target::builtin("x").build_std()));
            q.add(Crate::lib("b", "b").target(&Target::builtin("x")));
        });
        assert_eq!(
            errors,
            [
                "target \"x\" is defined twice: as builtin target \"x\" with build_std() \
                 and as builtin target \"x\""
            ]
        );

        let (_, errors) = lower(|q| {
            q.add(Crate::lib("a", "a").target(&Target::host().build_std()));
        });
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("Target::host() cannot use build_std()"));
    }

    #[test]
    fn opt_level_takes_sizes_and_reports_other_values() {
        let (raw, errors) = lower(|q| {
            q.add(Profile::release().opt_level("s"));
            q.add(Profile::new("tiny").opt_level("z"));
            q.add(Profile::new("fast").opt_level(OptLevel::Two));
        });
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(profile(&raw, "release").opt_level, Some(OptLevel::S));
        assert_eq!(profile(&raw, "tiny").opt_level, Some(OptLevel::Z));
        assert_eq!(profile(&raw, "fast").opt_level, Some(OptLevel::Two));

        let (_, errors) = lower(|q| {
            q.add(Profile::new("a").opt_level(4));
            q.add(Profile::new("b").opt_level("x"));
        });
        assert_eq!(
            errors,
            [
                "profile 'a': opt_level 4 is not 0, 1, 2, 3, \"s\" or \"z\"",
                "profile 'b': opt_level \"x\" is not 0, 1, 2, 3, \"s\" or \"z\""
            ]
        );
    }

    #[test]
    fn env_reaches_the_model_once_per_name() {
        let (raw, errors) = lower(|q| {
            q.env("DEFMT_LOG", "info");
            q.env("DEFMT_LOG", "info");
        });
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(raw.env["DEFMT_LOG"], "info");

        let (_, errors) = lower(|q| {
            q.env("DEFMT_LOG", "info");
            q.env("DEFMT_LOG", "debug");
            q.env("A=B", "x");
        });
        assert_eq!(
            errors,
            [
                "q.env: DEFMT_LOG is set twice (\"info\" and \"debug\")",
                "q.env: \"A=B\" is not a variable name (it must be non-empty, without '=' or NUL)"
            ]
        );
    }

    #[test]
    fn members_without_a_quon_process_to_ask_is_an_empty_list_and_an_error() {
        let mut q = Quon::new();
        assert!(q.cargo_workspace(".").members().is_empty());
        let (_, errors) = q.finish();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0]
                .message
                .starts_with("cargo_workspace(\".\").members(): needs quon"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn a_package_needs_a_workspace() {
        let mut q = Quon::new();
        q.add(Package::new("kernel"));
        let (_, errors) = q.finish();
        assert!(errors[0].message.contains("needs a Cargo workspace"));
    }

    #[test]
    fn esp_takes_crates_downloads_and_files() {
        let (raw, errors) = lower(|q| {
            let boot = q.add(Package::new("boot"));
            let efi = q.add(Download::limine("v11.4.1", "BOOTX64.EFI", ""));
            let disk = q.add(Image::new("disk"));
            q.add(
                Esp::new("esp")
                    .add(&boot, "EFI/BOOT/BOOTX64.EFI")
                    .add(&efi, "limine.efi")
                    .add("limine.conf", "limine.conf")
                    .add(&disk, "disk.img")
                    .add("x", "/abs"),
            );
        });
        let esp = raw.esps.get(raw.esps.lookup("esp").unwrap()).unwrap();
        let sources: Vec<_> = esp.entries.iter().map(|e| e.source.clone()).collect();
        assert_eq!(
            sources,
            [
                RawEspSource::Crate("boot".into()),
                RawEspSource::Download("limine/BOOTX64.EFI".into()),
                RawEspSource::File("limine.conf".into()),
            ]
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        let d = raw
            .downloads
            .get(raw.downloads.lookup("limine/BOOTX64.EFI").unwrap())
            .unwrap();
        assert!(d.url.ends_with("/v11.4.1-binary/BOOTX64.EFI"), "{}", d.url);
    }

    #[test]
    fn options_and_presets_are_typed() {
        let raw = ok(|q| {
            let log = q.add(opt::bool("LOG").default(true));
            let debug = q.add(opt::bool("DEBUG"));
            let level = q.add(opt::u32("LEVEL").default(3).range(0..=5));
            q.add(opt::bool("FANCY").depends_on(&log & !&debug & Cond::named("EXTRA")));
            q.add(opt::choice("ARCH", ["x86", "arm"]));
            q.add(Preset::new("verbose").set(&level, 5).set(&debug, true));
        });
        let opt = |n: &str| {
            raw.config_options
                .get(raw.config_options.lookup(n).unwrap())
                .unwrap()
        };
        assert_eq!(opt("LOG").default, ConfigValue::Bool(true));
        assert_eq!(opt("LEVEL").range, Some((0, 5)));
        assert_eq!(opt("ARCH").default, ConfigValue::Choice("x86".into()));
        assert_eq!(
            opt("FANCY").depends_on_expr,
            Some(Expr::And(vec![
                Expr::Ident("LOG".into()),
                Expr::Not(Box::new(Expr::Ident("DEBUG".into()))),
                Expr::Ident("EXTRA".into()),
            ]))
        );
        let p = &raw.presets["verbose"];
        assert_eq!(p.overrides["LEVEL"], ConfigValue::U32(5));
        assert_eq!(p.overrides["DEBUG"], ConfigValue::Bool(true));
    }

    #[test]
    fn plugin_runner_keeps_its_settings() {
        let raw = ok(|q| {
            q.add(
                Plugin::new("probe-rs")
                    .setting("chip", "nRF52840_xxAA")
                    .setting("speed", 4000),
            );
        });
        let r = raw
            .runners
            .get(raw.runners.lookup("probe-rs").unwrap())
            .unwrap();
        let Some(RunnerKind::Plugin(p)) = &r.kind else {
            panic!("{r:?}")
        };
        assert_eq!(
            serde_json::to_value(&p.settings).unwrap(),
            serde_json::json!({"chip": "nRF52840_xxAA", "speed": 4000})
        );
    }

    #[test]
    fn build_step_rejects_the_reserved_variable() {
        let (_, errors) = lower(|q| {
            let tool = q.add(Package::new("tool"));
            q.add(
                BuildStep::new("s", "out")
                    .tool(&tool)
                    .env("QUON_OUTPUT_PATH", "x"),
            );
        });
        assert!(errors[0].contains("reserved"), "{errors:?}");
    }
}
