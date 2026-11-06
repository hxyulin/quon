//! `gluon init`: write a `gluon.rs` for an existing Cargo workspace.
//!
//! [`infer`] reads the workspace manifests, `.cargo/config.toml`,
//! `rust-toolchain.toml` and `limine.conf` into a [`Plan`], and
//! [`render`] turns the plan into the file. What the files do not say
//! becomes a `// TODO:` comment holding the line to fill in, so the
//! generated file builds before anyone edits it.

use crate::cli::{Bootloader, InitArgs};
use anyhow::{Context, Result, bail};
use gluon_core::manifest::sources::RegistrySources;
use gluon_core::manifest::workspace::{PackageSet, load_workspace};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Run `gluon init` in the current directory.
pub fn run(args: &InitArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    run_at(&cwd, args)
}

/// Run `gluon init` against the workspace at `root`.
pub fn run_at(root: &Path, args: &InitArgs) -> Result<()> {
    let out = root.join("gluon.rs");
    if !args.dry_run && !args.force && out.exists() {
        bail!(
            "{} already exists; pass --force to overwrite it",
            out.display()
        );
    }
    let plan = infer(root, &Options::from(args))?;
    let text = render(&plan);
    if args.dry_run {
        print!("{text}");
        return Ok(());
    }
    std::fs::write(&out, &text).with_context(|| format!("failed to write {}", out.display()))?;
    let todos = text.matches("// TODO:").count();
    eprintln!("Wrote {}", out.display());
    if todos > 0 {
        eprintln!("{todos} `// TODO:` comments name what gluon init could not infer");
    }
    if plan
        .boot
        .as_ref()
        .is_some_and(|b| b.limine_version.is_some())
    {
        eprintln!(
            "The Limine hash is empty: the first build fails and prints it. \
             Check where the file came from, then pin it in gluon.rs."
        );
    }
    Ok(())
}

/// The flags that steer inference.
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub targets: Vec<String>,
    pub kernel: Option<String>,
    pub bootloader: Option<Bootloader>,
    pub limine: Option<String>,
}

impl From<&InitArgs> for Options {
    fn from(a: &InitArgs) -> Self {
        Self {
            targets: a.targets.clone(),
            kernel: a.kernel.clone(),
            bootloader: a.bootloader,
            limine: a.limine.clone(),
        }
    }
}

/// A target as `gluon.rs` declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The triple, or the file stem of a `.json` spec.
    pub name: String,
    /// The `.json` spec path, relative to the project root.
    pub spec: Option<String>,
}

impl Target {
    fn parse(s: &str) -> Self {
        if s.ends_with(".json") {
            let name = Path::new(s)
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| s.to_string());
            Self {
                name,
                spec: Some(s.to_string()),
            }
        } else {
            Self {
                name: s.to_string(),
                spec: None,
            }
        }
    }

    fn arch(&self) -> &str {
        self.name.split('-').next().unwrap_or(&self.name)
    }

    fn is_uefi(&self) -> bool {
        self.name.ends_with("-uefi")
    }

    /// The `gluon.rs` call that declares the target.
    fn declare(&self) -> String {
        match &self.spec {
            Some(spec) => format!("cfg.target_spec({:?}, {spec:?})", self.name),
            None => format!("cfg.target({:?})", self.name),
        }
    }

    fn var(&self) -> String {
        ident(&self.name)
    }
}

/// Where the linker script comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkerScript {
    /// One path, from a `-T` rustflag or the only `*.ld` next to the
    /// kernel's `Cargo.toml`.
    Path(String),
    /// The kernel's build script passes `-T` itself.
    BuildScript,
    /// Several `*.ld` files next to the kernel's `Cargo.toml`.
    Ambiguous(Vec<String>),
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kernel {
    pub package: String,
    /// The bin target, which `run_binary` and the ESP name.
    pub bin: String,
    /// The package directory, relative to the project root.
    pub dir: String,
    pub linker_script: LinkerScript,
}

/// Why no kernel was picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoKernel {
    NoBins,
    SeveralBins(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    pub bootloader: Bootloader,
    /// The UEFI boot file for the target's architecture, such as
    /// `BOOTX64.EFI`.
    pub efi_file: String,
    pub limine_version: Option<String>,
    /// The Limine config file at the project root, if there is one.
    pub limine_conf: Option<String>,
    /// Where the ESP holds the kernel: the path the Limine config names,
    /// else `boot/kernel`.
    pub kernel_path: String,
}

/// Everything [`render`] needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub name: String,
    pub version: String,
    /// The target every group builds for.
    pub target: Target,
    /// Where `target` came from, for the comment over it.
    pub target_source: &'static str,
    /// Other targets the toolchain files or flags name.
    pub other_targets: Vec<Target>,
    pub kernel: Result<Kernel, NoKernel>,
    /// Workspace members other than the kernel and proc macros.
    pub libraries: Vec<String>,
    /// Rustflags from `.cargo/config.toml` that Gluon does not read.
    pub rustflags: Vec<String>,
    pub boot: Option<Boot>,
}

/// Read the workspace at `root` into a [`Plan`].
pub fn infer(root: &Path, opts: &Options) -> Result<Plan> {
    if !root.join("Cargo.toml").is_file() {
        bail!(
            "no Cargo.toml in {}; gluon init needs an existing Cargo workspace",
            root.display()
        );
    }
    // Registry sources are not needed to list members and their
    // targets, and looking them up would run `cargo fetch`. Edges to
    // registry packages stay unresolved.
    let set = load_workspace(root, &RegistrySources::default())
        .with_context(|| format!("failed to read the Cargo workspace at {}", root.display()))?;
    let cargo_config = read_toml(&[root.join(".cargo/config.toml"), root.join(".cargo/config")])?;
    let toolchain = read_toml(&[
        root.join("rust-toolchain.toml"),
        root.join("rust-toolchain"),
    ])?;

    let bootloader = match (opts.bootloader, &opts.limine) {
        (None, Some(_)) => Bootloader::Limine,
        (Some(b), Some(_)) if b != Bootloader::Limine => {
            bail!("--limine needs --bootloader limine")
        }
        (b, _) => b.unwrap_or(Bootloader::None),
    };

    let (candidates, target_source) = if !opts.targets.is_empty() {
        (opts.targets.clone(), "--target")
    } else if let Some(t) = config_build_targets(cargo_config.as_ref()) {
        (t, ".cargo/config.toml")
    } else if let Some(t) = toolchain_targets(toolchain.as_ref()) {
        (t, "rust-toolchain.toml")
    } else {
        (Vec::new(), "")
    };
    let mut targets: Vec<Target> = Vec::new();
    for c in &candidates {
        let t = Target::parse(c);
        if !targets.contains(&t) {
            targets.push(t);
        }
    }
    let primary = pick_primary(&targets, bootloader, !opts.targets.is_empty());
    let (target, other_targets) = match primary {
        Some(i) => {
            let t = targets.remove(i);
            (t, targets)
        }
        None => (Target::parse("x86_64-unknown-none"), Vec::new()),
    };

    let mut rustflags = config_rustflags(cargo_config.as_ref(), &target.name);
    let flag_script = take_linker_script_flag(&mut rustflags);

    let kernel = pick_kernel(&set, opts.kernel.as_deref(), flag_script)?;
    let libraries = set
        .members
        .iter()
        .map(|&i| &set.packages[i])
        .filter(|p| !p.is_proc_macro())
        .filter(|p| kernel.as_ref().map_or(true, |k| p.name() != k.package))
        .map(|p| p.name().to_string())
        .collect();

    let boot = match bootloader {
        Bootloader::None => None,
        b => {
            if let Err(why) = &kernel {
                bail!(
                    "--bootloader {} needs a kernel, but {}; pass --kernel <package>",
                    if b == Bootloader::Limine {
                        "limine"
                    } else {
                        "uefi"
                    },
                    describe_no_kernel(why)
                );
            }
            let Some(efi_file) = efi_boot_file(target.arch()) else {
                bail!(
                    "no UEFI boot file name is known for the `{}` architecture of {}",
                    target.arch(),
                    target.name
                );
            };
            let (limine_conf, kernel_path) = if b == Bootloader::Limine {
                limine_config(root)?
            } else {
                (None, String::new())
            };
            Some(Boot {
                bootloader: b,
                efi_file: efi_file.to_string(),
                limine_version: opts.limine.clone(),
                limine_conf,
                kernel_path: if kernel_path.is_empty() {
                    "boot/kernel".to_string()
                } else {
                    kernel_path
                },
            })
        }
    };

    Ok(Plan {
        name: project_name(root),
        version: project_version(root)?,
        target,
        target_source,
        other_targets,
        kernel,
        libraries,
        rustflags,
        boot,
    })
}

/// Index of the target the groups build for. Explicit `--target` flags
/// keep their order; inferred lists prefer a UEFI target for a UEFI
/// kernel and a non-UEFI one otherwise, since `rust-toolchain.toml`
/// often lists a bootloader's UEFI target next to the kernel's.
fn pick_primary(targets: &[Target], bootloader: Bootloader, explicit: bool) -> Option<usize> {
    if targets.is_empty() {
        return None;
    }
    if explicit {
        return Some(0);
    }
    let want_uefi = bootloader == Bootloader::Uefi;
    targets
        .iter()
        .position(|t| t.is_uefi() == want_uefi)
        .or(Some(0))
}

fn read_toml(paths: &[PathBuf]) -> Result<Option<toml::Table>> {
    for p in paths {
        if p.is_file() {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("failed to read {}", p.display()))?;
            let table = text
                .parse::<toml::Table>()
                .with_context(|| format!("failed to parse {}", p.display()))?;
            return Ok(Some(table));
        }
    }
    Ok(None)
}

/// `build.target` from `.cargo/config.toml`: a string or an array.
fn config_build_targets(config: Option<&toml::Table>) -> Option<Vec<String>> {
    let v = config?.get("build")?.get("target")?;
    let list = string_or_array(v);
    (!list.is_empty()).then_some(list)
}

fn toolchain_targets(toolchain: Option<&toml::Table>) -> Option<Vec<String>> {
    let v = toolchain?.get("toolchain")?.get("targets")?;
    let list = string_or_array(v);
    (!list.is_empty()).then_some(list)
}

fn string_or_array(v: &toml::Value) -> Vec<String> {
    match v {
        toml::Value::String(s) => vec![s.clone()],
        toml::Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// `build.rustflags` and `target.<target>.rustflags`, split into flags.
/// `-C opt` pairs are joined into `-Copt`.
fn config_rustflags(config: Option<&toml::Table>, target: &str) -> Vec<String> {
    let Some(config) = config else {
        return Vec::new();
    };
    let mut raw = Vec::new();
    for v in [
        config.get("build").and_then(|b| b.get("rustflags")),
        config
            .get("target")
            .and_then(|t| t.get(target))
            .and_then(|t| t.get("rustflags")),
    ]
    .into_iter()
    .flatten()
    {
        match v {
            toml::Value::String(s) => raw.extend(s.split_whitespace().map(str::to_string)),
            other => raw.extend(string_or_array(other)),
        }
    }
    let mut flags = Vec::new();
    let mut it = raw.into_iter();
    while let Some(f) = it.next() {
        if f == "-C" {
            if let Some(next) = it.next() {
                flags.push(format!("-C{next}"));
            }
        } else {
            flags.push(f);
        }
    }
    flags
}

/// Remove a `-Clink-arg=-T<path>` flag and return its path.
fn take_linker_script_flag(flags: &mut Vec<String>) -> Option<String> {
    let i = flags.iter().position(|f| linker_script_of(f).is_some())?;
    let flag = flags.remove(i);
    linker_script_of(&flag).map(str::to_string)
}

fn linker_script_of(flag: &str) -> Option<&str> {
    let arg = flag
        .strip_prefix("-Clink-arg=")
        .or_else(|| flag.strip_prefix("-Clink-args="))?;
    let path = arg.strip_prefix("-T")?;
    (!path.is_empty()).then_some(path)
}

fn pick_kernel(
    set: &PackageSet,
    requested: Option<&str>,
    flag_script: Option<String>,
) -> Result<std::result::Result<Kernel, NoKernel>> {
    let with_bins: Vec<_> = set
        .members
        .iter()
        .map(|&i| &set.packages[i])
        .filter(|p| !p.bins.is_empty())
        .collect();
    let pkg = match requested {
        Some(name) => {
            let Some(i) = set.member(name) else {
                bail!(
                    "--kernel {name}: no workspace member is named `{name}`; members: {}",
                    names(set.members.iter().map(|&i| set.packages[i].name()))
                );
            };
            let pkg = &set.packages[i];
            if pkg.bins.is_empty() {
                bail!(
                    "--kernel {name}: `{name}` has no bin target; packages with one: {}",
                    names(with_bins.iter().map(|p| p.name()))
                );
            }
            pkg
        }
        None => match with_bins.as_slice() {
            [] => return Ok(Err(NoKernel::NoBins)),
            [one] => one,
            several => {
                return Ok(Err(NoKernel::SeveralBins(
                    several.iter().map(|p| p.name().to_string()).collect(),
                )));
            }
        },
    };
    let bin = match pkg.bins.as_slice() {
        [one] => one.name.clone(),
        bins => match bins.iter().find(|b| b.name == pkg.name()) {
            Some(b) => b.name.clone(),
            None => bail!(
                "package `{}` has several bin targets ({}) and none is named after it",
                pkg.name(),
                names(bins.iter().map(|b| b.name.as_str()))
            ),
        },
    };
    let dir = relative(&set.root, pkg.manifest_dir());
    let linker_script = match flag_script {
        Some(p) => LinkerScript::Path(p),
        None => find_linker_script(pkg.manifest_dir(), pkg.build_script.as_deref(), &dir)?,
    };
    Ok(Ok(Kernel {
        package: pkg.name().to_string(),
        bin,
        dir,
        linker_script,
    }))
}

fn find_linker_script(
    pkg_dir: &Path,
    build_script: Option<&Path>,
    rel_dir: &str,
) -> Result<LinkerScript> {
    if let Some(script) = build_script {
        let text = std::fs::read_to_string(pkg_dir.join(script)).unwrap_or_default();
        if text.contains("link-arg") && text.contains("-T") {
            return Ok(LinkerScript::BuildScript);
        }
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(pkg_dir)
        .with_context(|| format!("failed to read {}", pkg_dir.display()))?
    {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "ld") {
            let file = path.file_name().unwrap_or_default().to_string_lossy();
            found.push(join_rel(rel_dir, &file));
        }
    }
    found.sort();
    Ok(match found.len() {
        0 => LinkerScript::None,
        1 => LinkerScript::Path(found.remove(0)),
        _ => LinkerScript::Ambiguous(found),
    })
}

/// The Limine config file at the project root and the kernel path its
/// first entry names, without the `boot():/` prefix.
fn limine_config(root: &Path) -> Result<(Option<String>, String)> {
    for name in ["limine.conf", "limine.cfg"] {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        return Ok((Some(name.to_string()), limine_kernel_path(&text)));
    }
    Ok((None, String::new()))
}

/// The kernel path from `path: boot():/<p>` (Limine 8 and later) or
/// `KERNEL_PATH=boot:///<p>` (Limine 7). Empty when there is none.
fn limine_kernel_path(text: &str) -> String {
    for line in text.lines() {
        let line = line.trim();
        let value = match line.split_once(':') {
            Some((key, value))
                if matches!(
                    key.trim().to_ascii_lowercase().as_str(),
                    "path" | "kernel_path"
                ) =>
            {
                value.trim()
            }
            _ => match line.split_once('=') {
                Some((key, value)) if key.trim().eq_ignore_ascii_case("KERNEL_PATH") => {
                    value.trim()
                }
                _ => continue,
            },
        };
        for prefix in ["boot():/", "boot:///"] {
            if let Some(p) = value.strip_prefix(prefix) {
                return p.trim_start_matches('/').to_string();
            }
        }
    }
    String::new()
}

fn efi_boot_file(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "BOOTX64.EFI",
        "i386" | "i586" | "i686" => "BOOTIA32.EFI",
        "aarch64" => "BOOTAA64.EFI",
        "riscv64" | "riscv64gc" | "riscv64imac" => "BOOTRISCV64.EFI",
        _ => return None,
    })
}

/// QEMU program and machine for the architecture.
fn qemu_for(arch: &str) -> Option<(&'static str, &'static str)> {
    Some(match arch {
        "x86_64" => ("qemu-system-x86_64", "q35"),
        "i386" | "i586" | "i686" => ("qemu-system-i386", "q35"),
        "aarch64" => ("qemu-system-aarch64", "virt"),
        "riscv64" | "riscv64gc" | "riscv64imac" => ("qemu-system-riscv64", "virt"),
        _ => return None,
    })
}

fn project_name(root: &Path) -> String {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "kernel".to_string())
}

/// `workspace.package.version`, else the root package's literal
/// version, else `0.1.0`.
fn project_version(root: &Path) -> Result<String> {
    let manifest = read_toml(&[root.join("Cargo.toml")])?.unwrap_or_default();
    let ws = manifest
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str());
    let pkg = manifest
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str());
    Ok(ws.or(pkg).unwrap_or("0.1.0").to_string())
}

fn relative(root: &Path, dir: &Path) -> String {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    rel.to_string_lossy().replace('\\', "/")
}

fn join_rel(dir: &str, file: &str) -> String {
    if dir.is_empty() {
        file.to_string()
    } else {
        format!("{dir}/{file}")
    }
}

fn names<'a>(it: impl Iterator<Item = &'a str>) -> String {
    let v: Vec<_> = it.map(|n| format!("`{n}`")).collect();
    if v.is_empty() {
        "none".to_string()
    } else {
        v.join(", ")
    }
}

fn describe_no_kernel(why: &NoKernel) -> String {
    match why {
        NoKernel::NoBins => "no workspace member has a bin target".to_string(),
        NoKernel::SeveralBins(p) => format!(
            "several members have bin targets ({})",
            names(p.iter().map(String::as_str))
        ),
    }
}

/// A Rust identifier from a target name.
fn ident(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    s
}

/// Render the plan as a `gluon.rs`.
pub fn render(plan: &Plan) -> String {
    let mut o = String::new();
    let t = &plan.target;
    // The groups' target is named after its architecture; the targets
    // that only appear in comments keep the full name.
    let tv = ident(t.arch());

    let run_line = match plan.boot.as_ref().map(|b| b.bootloader) {
        Some(Bootloader::Limine) => {
            "//     gluon run       boot the kernel under QEMU through UEFI and Limine\n"
        }
        Some(Bootloader::Uefi) => {
            "//     gluon run       boot the kernel under QEMU through UEFI\n"
        }
        _ => "",
    };
    let _ = write!(
        o,
        "// Gluon build for {name}, written by `gluon init`.\n\
         //\n\
         //     gluon build     build every crate\n\
         //     gluon clippy    lint every crate\n\
         {run_line}\
         \n\
         use gluon_config::{{Config, Result}};\n\n",
        name = plan.name
    );

    let (list_name, list_doc) = match plan.kernel {
        Ok(_) => (
            "LIBRARIES",
            "Workspace library crates. The kernel package is declared on its own.",
        ),
        Err(_) => ("PACKAGES", "Workspace packages."),
    };
    if !plan.libraries.is_empty() {
        let quoted: Vec<_> = plan.libraries.iter().map(|n| format!("{n:?}")).collect();
        let _ = writeln!(
            o,
            "/// {list_doc}\nconst {list_name}: &[&str] = &[{}];\n",
            quoted.join(", ")
        );
    }

    o.push_str("pub fn config(cfg: &mut Config) -> Result<()> {\n");
    let _ = writeln!(o, "    cfg.project({:?}, {:?});", plan.name, plan.version);
    if plan.target_source.is_empty() {
        o.push_str(
            "    // TODO: no target found in .cargo/config.toml or rust-toolchain.toml.\n\
             \x20   // Replace the triple with the kernel's target, or a `.json` spec with\n\
             \x20   // cfg.target_spec(\"<name>\", \"<path>.json\").\n",
        );
    }
    let _ = writeln!(o, "    let {tv} = {};", t.declare());
    o.push_str("    cfg.cargo_workspace(\".\");\n\n");

    if !plan.other_targets.is_empty() {
        let source = plan.target_source;
        comment(
            &mut o,
            &format!(
                "TODO: {source} also names {}. Gluon builds each package for one target. \
                 To build packages for another, declare it and give it a group:",
                names(plan.other_targets.iter().map(|t| t.name.as_str()))
            ),
        );
        for other in &plan.other_targets {
            let _ = writeln!(o, "    // let {} = {};", other.var(), other.declare());
        }
        let first = &plan.other_targets[0];
        let _ = writeln!(
            o,
            "    // cfg.group({:?}).target({}).package(\"<package>\");\n",
            first.var(),
            first.var()
        );
    }

    // Profile.
    match &plan.kernel {
        Ok(_) => {}
        Err(why) => {
            let _ = writeln!(
                o,
                "    // TODO: {}. Name the kernel's bin target\n\
                 \x20   // so `gluon run` boots it, and declare its package in its own group:\n\
                 \x20   //     .run_binary(\"<bin>\")",
                capitalize(&describe_no_kernel(why))
            );
        }
    }
    let _ = write!(
        o,
        "    cfg.profile(\"dev\")\n        .target({tv})\n        .opt_level(0)\n        .debug_info(true)"
    );
    if let Ok(k) = &plan.kernel {
        let _ = write!(o, "\n        .run_binary({:?})", k.bin);
    }
    o.push_str(";\n\n");

    // Kernel group.
    if let Ok(k) = &plan.kernel {
        let static_reloc = plan.boot.as_ref().map(|b| b.bootloader) == Some(Bootloader::Limine)
            && t.name == "x86_64-unknown-none";
        if !plan.rustflags.is_empty() {
            let _ = writeln!(
                o,
                "    // TODO: .cargo/config.toml passes these rustflags, which Gluon does not\n\
                 \x20   // read. Add the ones the kernel needs to the kernel group:"
            );
            for f in &plan.rustflags {
                let _ = writeln!(o, "    //     .rustc_flag({f:?})");
            }
        }
        match &k.linker_script {
            LinkerScript::Path(_) => {}
            LinkerScript::BuildScript => {
                o.push_str("    // The kernel's build script passes the linker script.\n");
            }
            LinkerScript::None => {
                let _ = writeln!(
                    o,
                    "    // TODO: if the kernel needs a linker script, add it to the kernel group:\n\
                     \x20   //     .linker_script({:?})",
                    join_rel(&k.dir, "<name>.ld")
                );
            }
            LinkerScript::Ambiguous(paths) => {
                let _ = writeln!(
                    o,
                    "    // TODO: {} holds several linker scripts. Add the kernel's to the\n\
                     \x20   // kernel group, one of:",
                    if k.dir.is_empty() { "." } else { &k.dir }
                );
                for p in paths {
                    let _ = writeln!(o, "    //     .linker_script({p:?})");
                }
            }
        }
        if static_reloc {
            o.push_str(
                "    // x86_64-unknown-none links a position-independent executable by\n\
                 \x20   // default. Limine loads one only with a PT_DYNAMIC segment, so link a\n\
                 \x20   // plain executable at the address the linker script sets.\n",
            );
        }
        let _ = write!(
            o,
            "    cfg.group(\"kernel\")\n        .target({tv})\n        .package({:?})",
            k.package
        );
        if let LinkerScript::Path(p) = &k.linker_script {
            let _ = write!(o, "\n        .linker_script({p:?})");
        }
        if static_reloc {
            o.push_str("\n        .rustc_flag(\"-Crelocation-model=static\")");
        }
        o.push_str(";\n\n");
    }

    // Library group.
    if !plan.libraries.is_empty() {
        let (group, var) = match plan.kernel {
            Ok(_) => ("libraries", "libraries"),
            Err(_) => ("packages", "packages"),
        };
        comment(
            &mut o,
            &format!(
                "TODO: move a package that does not build for {} out of {list_name}.",
                t.name
            ),
        );
        let _ = writeln!(
            o,
            "    let mut {var} = cfg.group({group:?}).target({tv});\n\
             \x20   for name in {list_name} {{\n\
             \x20       {var}.package(name);\n\
             \x20   }}\n\
             \x20   drop({var});\n"
        );
    }

    // Boot.
    let arch = t.arch();
    match &plan.boot {
        None => render_qemu_commented(&mut o, arch),
        Some(boot) => {
            let kernel = plan
                .kernel
                .as_ref()
                .expect("infer requires a kernel to boot");
            render_esp(&mut o, boot, &kernel.bin);
            render_qemu(&mut o, arch);
        }
    }

    o.push_str("    Ok(())\n}\n");
    o
}

fn render_esp(o: &mut String, boot: &Boot, bin: &str) {
    let efi = &boot.efi_file;
    match boot.bootloader {
        Bootloader::Uefi => {
            let _ = writeln!(
                o,
                "    // UEFI firmware starts EFI/BOOT/{efi}.\n\
                 \x20   cfg.esp(\"default\").add({bin:?}, \"EFI/BOOT/{efi}\");\n"
            );
        }
        Bootloader::Limine => {
            let limine_line =
                |version: &str| format!("cfg.limine({version:?}).file({efi:?}, \"\");");
            let download = format!(".add_download(\"limine/{efi}\", \"EFI/BOOT/{efi}\")");
            match &boot.limine_version {
                Some(v) => {
                    let _ = writeln!(
                        o,
                        "    // To upgrade Limine, change the version and the hash. An empty hash\n\
                         \x20   // makes the build fail and print the hash of the file it fetched.\n\
                         \x20   // Check where the file came from before you pin it.\n\
                         \x20   {}\n",
                        limine_line(v)
                    );
                }
                None => {
                    let _ = writeln!(
                        o,
                        "    // TODO: pin a Limine release. Uncomment this line and the\n\
                         \x20   // add_download line below, and set the version. The first build\n\
                         \x20   // fails and prints the hash to fill in.\n\
                         \x20   // {}\n",
                        limine_line("<version>")
                    );
                }
            }
            let conf = boot.limine_conf.as_deref();
            let _ = writeln!(
                o,
                "    // UEFI firmware starts EFI/BOOT/{efi}. Limine reads {} from\n\
                 \x20   // the ESP root, and it names {}.",
                conf.unwrap_or("limine.conf"),
                boot.kernel_path
            );
            if conf.is_none() {
                let _ = writeln!(
                    o,
                    "    // TODO: write limine.conf next to this file, with an entry whose path\n\
                     \x20   // is boot():/{}, and uncomment the add_file line.",
                    boot.kernel_path
                );
            }
            o.push_str("    cfg.esp(\"default\")\n");
            if boot.limine_version.is_some() {
                let _ = writeln!(o, "        {download}");
            } else {
                let _ = writeln!(o, "        // {download}");
            }
            match conf {
                Some(c) => {
                    let _ = writeln!(o, "        .add_file({c:?}, {c:?})");
                }
                None => o.push_str("        // .add_file(\"limine.conf\", \"limine.conf\")\n"),
            }
            let _ = writeln!(o, "        .add({bin:?}, {:?});\n", boot.kernel_path);
        }
        Bootloader::None => {}
    }
}

fn render_qemu(o: &mut String, arch: &str) {
    let Some((program, machine)) = qemu_for(arch) else {
        let _ = writeln!(
            o,
            "    // TODO: no QEMU machine is known for `{arch}`. Declare QEMU to use\n\
             \x20   // `gluon run`:\n\
             \x20   // cfg.qemu(\"qemu-system-<arch>\").machine(\"<machine>\").boot_mode(\"uefi\");\n"
        );
        return;
    };
    if program == "qemu-system-x86_64" || program == "qemu-system-i386" {
        o.push_str(
            "    // TODO: if the kernel exits QEMU through isa-debug-exit, add the device and\n\
             \x20   // the status QEMU then exits with, (<value> << 1) | 1, to cfg.qemu:\n\
             \x20   //     .extra_args(&[\"-device\", \"isa-debug-exit,iobase=0xf4,iosize=0x04\"])\n\
             \x20   //     .success_exit_code(<status>)\n",
        );
    }
    let _ = writeln!(
        o,
        "    cfg.qemu({program:?})\n\
         \x20       .machine({machine:?})\n\
         \x20       .memory(256)\n\
         \x20       .cores(1)\n\
         \x20       .serial_stdio()\n\
         \x20       .boot_mode(\"uefi\");\n"
    );
}

fn render_qemu_commented(o: &mut String, arch: &str) {
    let (program, machine) = qemu_for(arch).unwrap_or(("qemu-system-<arch>", "<machine>"));
    let _ = writeln!(
        o,
        "    // TODO: to boot the kernel with `gluon run`, uncomment this block, or run\n\
         \x20   // `gluon init --force --bootloader limine` (or `uefi`) to boot through UEFI:\n\
         \x20   // cfg.qemu({program:?})\n\
         \x20   //     .machine({machine:?})\n\
         \x20   //     .memory(256)\n\
         \x20   //     .cores(1)\n\
         \x20   //     .serial_stdio();\n"
    );
}

/// Write `text` as `//` comment lines indented by four spaces, wrapped
/// at 80 columns.
fn comment(o: &mut String, text: &str) {
    const PREFIX: &str = "    //";
    let mut line = String::from(PREFIX);
    for word in text.split_whitespace() {
        if line.len() > PREFIX.len() && line.len() + 1 + word.len() > 80 {
            o.push_str(&line);
            o.push('\n');
            line = String::from(PREFIX);
        }
        line.push(' ');
        line.push_str(word);
    }
    o.push_str(&line);
    o.push('\n');
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write `files` (path, contents) under a fresh tempdir.
    fn workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        for (path, text) in files {
            let p = tmp.path().join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        tmp
    }

    const ROOT: &str = "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n\
                        [workspace.package]\nversion = \"0.3.0\"\nedition = \"2024\"\n";
    const KERNEL: &str = "[package]\nname = \"kernel\"\nversion.workspace = true\n\
                          edition.workspace = true\n";
    const LIB: &str = "[package]\nname = \"util\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

    /// A kernel package with `src/main.rs`, a linker script next to its
    /// manifest, and one library.
    fn kernel_workspace(extra: &[(&str, &str)]) -> tempfile::TempDir {
        let mut files = vec![
            ("Cargo.toml", ROOT),
            ("crates/kernel/Cargo.toml", KERNEL),
            ("crates/kernel/src/main.rs", "fn main() {}\n"),
            ("crates/kernel/kernel.ld", "ENTRY(_start)\n"),
            ("crates/util/Cargo.toml", LIB),
            ("crates/util/src/lib.rs", ""),
        ];
        files.extend_from_slice(extra);
        workspace(&files)
    }

    fn opts() -> Options {
        Options::default()
    }

    #[test]
    fn infers_kernel_linker_script_version_and_libraries() {
        let ws = kernel_workspace(&[(
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"nightly\"\ntargets = [\"x86_64-unknown-none\"]\n",
        )]);
        let plan = infer(ws.path(), &opts()).unwrap();
        assert_eq!(plan.version, "0.3.0");
        assert_eq!(plan.target, Target::parse("x86_64-unknown-none"));
        assert_eq!(plan.target_source, "rust-toolchain.toml");
        let k = plan.kernel.as_ref().unwrap();
        assert_eq!((k.package.as_str(), k.bin.as_str()), ("kernel", "kernel"));
        assert_eq!(
            k.linker_script,
            LinkerScript::Path("crates/kernel/kernel.ld".into())
        );
        assert_eq!(plan.libraries, ["util"]);
        assert_eq!(plan.boot, None);
        assert_eq!(plan.name, project_name(ws.path()));
    }

    #[test]
    fn cargo_config_target_wins_over_toolchain_and_reads_json_specs() {
        let ws = kernel_workspace(&[
            (
                "rust-toolchain.toml",
                "[toolchain]\ntargets = [\"aarch64-unknown-none\"]\n",
            ),
            (
                ".cargo/config.toml",
                "[build]\ntarget = \"targets/x86_64-myos.json\"\n",
            ),
        ]);
        let plan = infer(ws.path(), &opts()).unwrap();
        assert_eq!(plan.target_source, ".cargo/config.toml");
        assert_eq!(plan.target.name, "x86_64-myos");
        assert_eq!(
            plan.target.spec.as_deref(),
            Some("targets/x86_64-myos.json")
        );
        assert!(plan.other_targets.is_empty());
        assert!(render(&plan).contains(
            "let x86_64 = cfg.target_spec(\"x86_64-myos\", \"targets/x86_64-myos.json\");"
        ));
    }

    #[test]
    fn target_flags_override_inference_and_keep_their_order() {
        let ws = kernel_workspace(&[(
            "rust-toolchain.toml",
            "[toolchain]\ntargets = [\"x86_64-unknown-none\"]\n",
        )]);
        let o = Options {
            targets: vec!["x86_64-unknown-uefi".into(), "aarch64-unknown-none".into()],
            ..opts()
        };
        let plan = infer(ws.path(), &o).unwrap();
        assert_eq!(plan.target_source, "--target");
        assert_eq!(plan.target.name, "x86_64-unknown-uefi");
        assert_eq!(plan.other_targets, [Target::parse("aarch64-unknown-none")]);
    }

    #[test]
    fn inferred_targets_put_a_uefi_target_first_only_for_a_uefi_kernel() {
        let toolchain =
            "[toolchain]\ntargets = [\"x86_64-unknown-uefi\", \"x86_64-unknown-none\"]\n";
        let ws = kernel_workspace(&[("rust-toolchain.toml", toolchain)]);
        let plan = infer(ws.path(), &opts()).unwrap();
        assert_eq!(plan.target.name, "x86_64-unknown-none");
        assert_eq!(plan.other_targets, [Target::parse("x86_64-unknown-uefi")]);

        let o = Options {
            bootloader: Some(Bootloader::Uefi),
            ..opts()
        };
        let plan = infer(ws.path(), &o).unwrap();
        assert_eq!(plan.target.name, "x86_64-unknown-uefi");
        let boot = plan.boot.as_ref().unwrap();
        assert_eq!(boot.efi_file, "BOOTX64.EFI");
        assert!(
            render(&plan)
                .contains("cfg.esp(\"default\").add(\"kernel\", \"EFI/BOOT/BOOTX64.EFI\");")
        );
    }

    #[test]
    fn no_target_anywhere_leaves_a_todo() {
        let ws = kernel_workspace(&[]);
        let plan = infer(ws.path(), &opts()).unwrap();
        assert_eq!(plan.target_source, "");
        let text = render(&plan);
        assert!(text.contains("// TODO: no target found"), "{text}");
        assert!(text.contains("let x86_64 = cfg.target(\"x86_64-unknown-none\");"));
    }

    #[test]
    fn linker_script_flag_in_cargo_config_wins_and_other_flags_become_a_todo() {
        let ws = kernel_workspace(&[(
            ".cargo/config.toml",
            "[build]\ntarget = \"x86_64-unknown-none\"\n\n\
             [target.x86_64-unknown-none]\n\
             rustflags = [\"-C\", \"link-arg=-Tlink/kernel.ld\", \"-Cforce-frame-pointers=yes\"]\n",
        )]);
        let plan = infer(ws.path(), &opts()).unwrap();
        let k = plan.kernel.as_ref().unwrap();
        assert_eq!(k.linker_script, LinkerScript::Path("link/kernel.ld".into()));
        assert_eq!(plan.rustflags, ["-Cforce-frame-pointers=yes"]);
        let text = render(&plan);
        assert!(
            text.contains(".linker_script(\"link/kernel.ld\")"),
            "{text}"
        );
        assert!(
            text.contains("//     .rustc_flag(\"-Cforce-frame-pointers=yes\")"),
            "{text}"
        );
    }

    #[test]
    fn linker_script_from_build_script_is_not_repeated() {
        let ws = kernel_workspace(&[(
            "crates/kernel/build.rs",
            "fn main() { println!(\"cargo:rustc-link-arg=-Tcrates/kernel/kernel.ld\"); }\n",
        )]);
        let plan = infer(ws.path(), &opts()).unwrap();
        assert_eq!(
            plan.kernel.as_ref().unwrap().linker_script,
            LinkerScript::BuildScript
        );
        assert!(!render(&plan).contains(".linker_script("));
    }

    #[test]
    fn several_or_no_linker_scripts_leave_a_todo() {
        let ws = kernel_workspace(&[("crates/kernel/other.ld", "")]);
        let plan = infer(ws.path(), &opts()).unwrap();
        assert_eq!(
            plan.kernel.as_ref().unwrap().linker_script,
            LinkerScript::Ambiguous(vec![
                "crates/kernel/kernel.ld".into(),
                "crates/kernel/other.ld".into()
            ])
        );
        let text = render(&plan);
        assert!(text.contains("//     .linker_script(\"crates/kernel/other.ld\")"));

        let ws = workspace(&[
            ("Cargo.toml", ROOT),
            ("crates/kernel/Cargo.toml", KERNEL),
            ("crates/kernel/src/main.rs", "fn main() {}\n"),
        ]);
        let plan = infer(ws.path(), &opts()).unwrap();
        assert_eq!(
            plan.kernel.as_ref().unwrap().linker_script,
            LinkerScript::None
        );
        assert!(render(&plan).contains("//     .linker_script(\"crates/kernel/<name>.ld\")"));
    }

    #[test]
    fn several_bin_packages_leave_the_kernel_to_a_todo() {
        let ws = kernel_workspace(&[
            (
                "crates/tool/Cargo.toml",
                "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            ("crates/tool/src/main.rs", "fn main() {}\n"),
        ]);
        let plan = infer(ws.path(), &opts()).unwrap();
        assert_eq!(
            plan.kernel,
            Err(NoKernel::SeveralBins(vec!["kernel".into(), "tool".into()]))
        );
        assert_eq!(plan.libraries, ["kernel", "tool", "util"]);
        let text = render(&plan);
        assert!(text.contains("//     .run_binary(\"<bin>\")"), "{text}");
        assert!(!text.contains("cfg.group(\"kernel\")"));
        assert!(text.contains("const PACKAGES"));

        // A boot setup needs to know the kernel.
        let o = Options {
            bootloader: Some(Bootloader::Limine),
            ..opts()
        };
        let err = infer(ws.path(), &o).unwrap_err().to_string();
        assert!(err.contains("--kernel"), "{err}");

        // --kernel picks one.
        let o = Options {
            kernel: Some("tool".into()),
            ..opts()
        };
        let plan = infer(ws.path(), &o).unwrap();
        assert_eq!(plan.kernel.as_ref().unwrap().package, "tool");
        assert_eq!(plan.libraries, ["kernel", "util"]);
    }

    #[test]
    fn kernel_flag_must_name_a_member_with_a_bin() {
        let ws = kernel_workspace(&[]);
        for (name, want) in [("nope", "no workspace member"), ("util", "no bin target")] {
            let o = Options {
                kernel: Some(name.into()),
                ..opts()
            };
            let err = infer(ws.path(), &o).unwrap_err().to_string();
            assert!(err.contains(want), "{name}: {err}");
        }
    }

    #[test]
    fn limine_reads_the_kernel_path_from_limine_conf() {
        let ws = kernel_workspace(&[(
            "limine.conf",
            "timeout: 0\n\n/My OS\n    protocol: limine\n    path: boot():/os/kernel.elf\n",
        )]);
        let o = Options {
            limine: Some("11.4.1".into()),
            ..opts()
        };
        let plan = infer(ws.path(), &o).unwrap();
        let boot = plan.boot.as_ref().unwrap();
        assert_eq!(boot.bootloader, Bootloader::Limine);
        assert_eq!(boot.limine_conf.as_deref(), Some("limine.conf"));
        assert_eq!(boot.kernel_path, "os/kernel.elf");
        let text = render(&plan);
        assert!(text.contains("cfg.limine(\"11.4.1\").file(\"BOOTX64.EFI\", \"\");"));
        assert!(
            text.contains(
                "        .add_download(\"limine/BOOTX64.EFI\", \"EFI/BOOT/BOOTX64.EFI\")"
            )
        );
        assert!(text.contains(".add_file(\"limine.conf\", \"limine.conf\")"));
        assert!(text.contains(".add(\"kernel\", \"os/kernel.elf\");"));
        assert!(text.contains(".rustc_flag(\"-Crelocation-model=static\")"));
        assert!(text.contains(".boot_mode(\"uefi\")"));
    }

    #[test]
    fn limine_without_a_version_or_config_comments_those_lines_out() {
        let ws = kernel_workspace(&[]);
        let o = Options {
            bootloader: Some(Bootloader::Limine),
            ..opts()
        };
        let plan = infer(ws.path(), &o).unwrap();
        let text = render(&plan);
        assert!(text.contains("    // cfg.limine(\"<version>\").file(\"BOOTX64.EFI\", \"\");"));
        assert!(text.contains("        // .add_download("));
        assert!(text.contains("        // .add_file(\"limine.conf\", \"limine.conf\")"));
        assert!(text.contains(".add(\"kernel\", \"boot/kernel\");"));
    }

    #[test]
    fn limine_kernel_path_reads_both_config_syntaxes() {
        assert_eq!(
            limine_kernel_path("/x\n    PATH: boot():/boot/kernel\n"),
            "boot/kernel"
        );
        assert_eq!(
            limine_kernel_path("/x\n  path: boot():/boot/kernel\n"),
            "boot/kernel"
        );
        assert_eq!(
            limine_kernel_path(":x\n  KERNEL_PATH=boot:///boot/kernel\n"),
            "boot/kernel"
        );
        assert_eq!(limine_kernel_path("timeout: 0\n"), "");
    }

    #[test]
    fn limine_flag_conflicts_with_another_bootloader() {
        let ws = kernel_workspace(&[]);
        let o = Options {
            bootloader: Some(Bootloader::Uefi),
            limine: Some("11.4.1".into()),
            ..opts()
        };
        assert!(infer(ws.path(), &o).is_err());
    }

    #[test]
    fn bootloader_none_comments_out_qemu() {
        let ws = kernel_workspace(&[]);
        let text = render(&infer(ws.path(), &opts()).unwrap());
        assert!(
            text.contains("    // cfg.qemu(\"qemu-system-x86_64\")"),
            "{text}"
        );
        assert!(!text.contains("    cfg.qemu("));
        assert!(!text.contains("cfg.esp("));
    }

    #[test]
    fn proc_macro_members_are_left_out() {
        let ws = kernel_workspace(&[
            (
                "crates/derive/Cargo.toml",
                "[package]\nname = \"derive\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [lib]\nproc-macro = true\n",
            ),
            ("crates/derive/src/lib.rs", ""),
        ]);
        let plan = infer(ws.path(), &opts()).unwrap();
        assert_eq!(plan.libraries, ["util"]);
    }

    #[test]
    fn refuses_to_overwrite_without_force() {
        let ws = kernel_workspace(&[("gluon.rs", "// mine\n")]);
        let args = |force| InitArgs {
            targets: Vec::new(),
            kernel: None,
            bootloader: None,
            limine: None,
            force,
            dry_run: false,
        };
        let err = run_at(ws.path(), &args(false)).unwrap_err().to_string();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(
            fs::read_to_string(ws.path().join("gluon.rs")).unwrap(),
            "// mine\n"
        );

        run_at(ws.path(), &args(true)).unwrap();
        let text = fs::read_to_string(ws.path().join("gluon.rs")).unwrap();
        assert!(text.contains("pub fn config(cfg: &mut Config)"));
    }

    #[test]
    fn comment_wraps_at_80_columns() {
        let mut o = String::new();
        comment(&mut o, &"word ".repeat(30));
        assert!(o.lines().count() > 1);
        assert!(
            o.lines().all(|l| l.len() <= 80 && l.starts_with("    // ")),
            "{o}"
        );
    }

    #[test]
    fn rejects_a_directory_without_cargo_toml() {
        let ws = workspace(&[]);
        let err = infer(ws.path(), &opts()).unwrap_err().to_string();
        assert!(err.contains("existing Cargo workspace"), "{err}");
    }
}
