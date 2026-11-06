//! Profile + config option resolution. See [`resolve`].

use crate::config::interpolate::interpolate;
use crate::error::{Diagnostic, Error, Result};
use quon_model::{
    BuildModel, BuildStepInput, ConfigOptionDef, ConfigType, ConfigValue, CrateDef, CrateType,
    DepResolution, Expr, Handle, ImageSource, ProfileDef, RawSource, ResolvedConfig,
    ResolvedCrateRef, ResolvedProfile, ResolvedValue, TargetDef, TargetSpec, TristateVal,
};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::Path;

/// Maximum number of select/depends iteration passes before declaring
/// non-convergence (almost always indicates a misuse — well-formed
/// configurations converge in 2-3 iterations).
const MAX_FIXEDPOINT_ITERS: usize = 32;

/// Resolve a [`BuildModel`] into a [`ResolvedConfig`] for a specific profile.
///
/// - `profile_name`: the profile to build (e.g. `"default"`).
/// - `target_override`: if `Some`, overrides whatever target the profile
///   declared. Errors if the named target does not exist.
/// - `project_root`: used as the base for `build_dir` (always
///   `project_root.join("build")`).
/// - `option_overrides`: caller-provided per-option values, typically
///   loaded from `.quon-config`. Applied **after** preset overrides but
///   **before** the selects/depends fixed point. `None` means "no
///   overrides; use the profile's preset (if any) and option defaults".
///
/// Returns a fully validated, flattened [`ResolvedConfig`] ready for the
/// scheduler. On any failure (missing profile, unknown target, range
/// violation, type mismatch, interpolation cycle, etc.) returns
/// `Err(Error::Diagnostics(...))` carrying every problem detected during
/// the pass — matching the intern/validate accumulation pattern.
pub fn resolve(
    model: &BuildModel,
    profile_name: &str,
    target_override: Option<&str>,
    project_root: &Path,
    option_overrides: Option<&BTreeMap<String, ConfigValue>>,
) -> Result<ResolvedConfig> {
    let mut diags: Vec<Diagnostic> = Vec::new();

    // 1. Look up the profile.
    let Some(profile_handle) = model.profiles.lookup(profile_name) else {
        return Err(Error::Diagnostics(vec![Diagnostic::error(format!(
            "unknown profile {profile_name:?}"
        ))]));
    };

    // 2. Walk inheritance and flatten.
    let flattened = match flatten_profile(model, profile_handle) {
        Ok(p) => p,
        Err(d) => {
            diags.push(d);
            return Err(Error::Diagnostics(diags));
        }
    };

    // 3. Resolve effective target.
    let effective_target = match target_override {
        Some(name) => match model.targets.lookup(name) {
            Some(h) => Some(TargetSpec::Cross(h)),
            None => {
                diags.push(Diagnostic::error(format!(
                    "--target override '{name}' does not match any target defined in the build model"
                )));
                None
            }
        },
        None => flattened.target,
    };
    let Some(effective_target) = effective_target else {
        diags.push(Diagnostic::error(format!(
            "profile '{profile_name}' does not specify a target and no target override was provided"
        )));
        return Err(Error::Diagnostics(diags));
    };

    // 4. opt_level / debug_info / lto / run_binary with defaults.
    // quon-config rejects a level rustc does not take, so any value
    // here is valid.
    let opt_level = flattened.opt_level.unwrap_or_default();
    let debug_info = flattened.debug_info.unwrap_or(false);
    // Unset, both follow rustc's own defaults, so a profile that names
    // neither builds as rustc would without the flags.
    let debug_assertions = flattened
        .debug_assertions
        .unwrap_or(opt_level == quon_model::OptLevel::Zero);
    let overflow_checks = flattened.overflow_checks.unwrap_or(debug_assertions);
    let lto = flattened.lto.clone();
    let run_binary = flattened.run_binary;

    // 5. Resolve config options (defaults → preset → overrides).
    let mut resolved_options: BTreeMap<String, ResolvedValue> = BTreeMap::new();
    let preset_overrides: Option<&BTreeMap<String, ConfigValue>> = flattened
        .preset
        .as_ref()
        .and_then(|name| model.presets.get(name))
        .map(|p| &p.overrides);

    for (_, opt) in model.config_options.iter() {
        // Skip group structural options — they don't carry values.
        if matches!(opt.ty, ConfigType::Group) {
            continue;
        }
        let name = &opt.name;

        let mut value =
            coerce(&opt.default, opt, name, &mut diags).unwrap_or_else(|| off_value_for(opt));

        if let Some(preset_map) = preset_overrides
            && let Some(pv) = preset_map.get(name)
            && let Some(coerced) = coerce_with_label(
                pv,
                opt,
                name,
                &mut diags,
                &format!("preset '{}'", flattened.preset.as_deref().unwrap_or("?")),
            )
        {
            value = coerced;
        }

        if let Some(overrides) = option_overrides
            && let Some(ov) = overrides.get(name)
            && let Some(coerced) = coerce_with_label(ov, opt, name, &mut diags, "config override")
        {
            value = coerced;
        }

        resolved_options.insert(name.clone(), value);
    }

    // 6. Validate ranges.
    for (_, opt) in model.config_options.iter() {
        let Some((lo, hi)) = opt.range else { continue };
        let name = &opt.name;
        let Some(v) = resolved_options.get(name) else {
            continue;
        };
        let n: u64 = match v {
            ResolvedValue::U32(n) => *n as u64,
            ResolvedValue::U64(n) => *n,
            _ => continue,
        };
        if n < lo || n > hi {
            diags.push(Diagnostic::error(format!(
                "option '{name}' value {n} is outside declared range [{lo}, {hi}]"
            )));
        }
    }

    // 7. Validate choice values.
    for (_, opt) in model.config_options.iter() {
        if !matches!(opt.ty, ConfigType::Choice) {
            continue;
        }
        let Some(choices) = &opt.choices else {
            continue;
        };
        let name = &opt.name;
        let Some(ResolvedValue::Choice(picked)) = resolved_options.get(name) else {
            continue;
        };
        if !choices.iter().any(|c| c == picked) {
            diags.push(Diagnostic::error(format!(
                "option '{name}' value '{picked}' is not in declared choices [{}]",
                choices.join(", ")
            )));
        }
    }

    // 8. Selects/depends_on fixed point.
    let mut converged = false;
    for _ in 0..MAX_FIXEDPOINT_ITERS {
        let mut changed = false;

        // depends_on: any unsatisfied dependency forces this option off.
        // Collect names first to avoid mutation during iteration.
        let names: Vec<String> = model
            .config_options
            .iter()
            .map(|(_, o)| o.name.clone())
            .collect();
        for name in &names {
            let Some(opt) = model
                .config_options
                .lookup(name)
                .and_then(|h| model.config_options.get(h))
            else {
                continue;
            };

            // Single path: evaluate `depends_on_expr` if present. Both
            // declaration surfaces — the `.kconfig` loader and the Rhai
            // `.depends_on(...)` / `.depends_on_expr(...)` builders —
            // populate this Expr tree. For the common case of a plain
            // AND-of-idents (which is what `.depends_on([A, B])` and a
            // `.kconfig` `depends_on = A && B` both produce), we walk
            // the tree to name the first unsatisfied ident in the
            // diagnostic so the user isn't left staring at a generic
            // "expression not satisfied" message.
            if let Some(expr) = &opt.depends_on_expr {
                let lookup = |n: &str| resolved_options.get(n).map(is_on);
                if !expr.eval(&lookup) {
                    let off = off_value_for(opt);
                    let cur = resolved_options.get(name);
                    if cur != Some(&off) {
                        let diag = if let Some(dep_name) = first_unsatisfied_ident(expr, &lookup) {
                            Diagnostic::error(format!(
                                "option '{name}' disabled because dependency '{dep_name}' is not satisfied"
                            ))
                            .with_note("depends_on forces an option to its 'off' value when any dependency is off")
                        } else {
                            Diagnostic::error(format!(
                                "option '{name}' disabled because its 'depends_on' expression is not satisfied"
                            ))
                            .with_note("depends_on_expr evaluates with `&&`/`||`/`!` semantics; check that the referenced options are set to the values you expect")
                        };
                        diags.push(diag);
                        resolved_options.insert(name.clone(), off);
                        changed = true;
                    }
                }
            }
        }

        // selects: any "on" option forces its selects to "on".
        for name in &names {
            let Some(opt) = model
                .config_options
                .lookup(name)
                .and_then(|h| model.config_options.get(h))
            else {
                continue;
            };
            if opt.selects.is_empty() {
                continue;
            }
            let on = resolved_options.get(name).map(is_on).unwrap_or(false);
            if !on {
                continue;
            }
            for sel in &opt.selects {
                let Some(target_opt) = model
                    .config_options
                    .lookup(sel)
                    .and_then(|h| model.config_options.get(h))
                else {
                    diags.push(Diagnostic::error(format!(
                        "option '{name}' selects unknown option '{sel}'"
                    )));
                    continue;
                };
                let on_value = on_value_for(target_opt);
                let cur = resolved_options.get(sel);
                if cur != Some(&on_value) {
                    resolved_options.insert(sel.clone(), on_value);
                    changed = true;
                }
            }
        }

        if !changed {
            converged = true;
            break;
        }
    }
    if !converged {
        diags.push(Diagnostic::error(
            "config option resolution did not converge (likely a selects cycle)",
        ));
    }

    // 9. String interpolation pass.
    let snapshot = resolved_options.clone();
    let mut updates: Vec<(String, ResolvedValue)> = Vec::new();
    for (name, value) in &resolved_options {
        let new = match value {
            ResolvedValue::String(s) if s.contains("${") => {
                let mut visiting = HashSet::new();
                visiting.insert(name.clone());
                match interpolate(s, &snapshot, &mut visiting) {
                    Ok(expanded) => Some(ResolvedValue::String(expanded)),
                    Err(e) => {
                        diags.push(Diagnostic::error(format!(
                            "while interpolating option '{name}': {e}"
                        )));
                        None
                    }
                }
            }
            ResolvedValue::Choice(s) if s.contains("${") => {
                let mut visiting = HashSet::new();
                visiting.insert(name.clone());
                match interpolate(s, &snapshot, &mut visiting) {
                    Ok(expanded) => Some(ResolvedValue::Choice(expanded)),
                    Err(e) => {
                        diags.push(Diagnostic::error(format!(
                            "while interpolating option '{name}': {e}"
                        )));
                        None
                    }
                }
            }
            _ => None,
        };
        if let Some(nv) = new {
            updates.push((name.clone(), nv));
        }
    }
    for (k, v) in updates {
        resolved_options.insert(k, v);
    }

    // 10. Resolve crate list. When run_binary is set, include the crates
    //     of the profile's target and everything reachable from them and
    //     from the run binary via deps + artifact_deps closure, plus the
    //     host test executables and their deps. This
    //     prevents wasteful compilation: a project with profile("x86")
    //     and profile("arm") will not compile the arm crates for the x86
    //     profile, but every crate declared for x86 still builds.
    //
    //     When run_binary is None, fall back to including every crate
    //     (backward compat for simple single-target projects).
    //
    //     Host crates (target == "host") set `host: true` so the
    //     scheduler compiles them for the build machine. All other crates
    //     are cross and receive their group's resolved target handle.
    let reachable = run_binary.map(|root| reachable_crates(model, root, effective_target));
    // Host crates carry a target handle the scheduler never reads (see
    // `ResolvedCrateRef::target`). A cross profile fills it with its
    // target; a host profile may declare no target at all, so it gets a
    // handle that resolves to nothing.
    let placeholder = match effective_target {
        TargetSpec::Cross(t) => t,
        TargetSpec::Host => Handle::new(u32::MAX),
    };
    let mut resolved_crates: Vec<ResolvedCrateRef> = Vec::new();
    for (crate_handle, krate) in model.crates.iter() {
        // Filter to reachable crates when run_binary is set.
        if let Some(ref set) = reachable
            && !set.contains(&crate_handle)
        {
            continue;
        }
        let Some(group) = model.groups.get(krate.group) else {
            continue;
        };
        let is_host = matches!(group.target, TargetSpec::Host);
        // For host crates, the target field is just a placeholder — the
        // scheduler keys off `host: bool`. We use `effective_target` as a
        // safe placeholder so the field is always populated with a valid
        // arena handle.
        let target = match krate.target {
            TargetSpec::Cross(h) if !is_host => h,
            _ => placeholder,
        };
        resolved_crates.push(ResolvedCrateRef {
            handle: crate_handle,
            target,
            host: is_host,
        });
    }

    if !diags.is_empty() {
        return Err(Error::Diagnostics(diags));
    }

    // 11. Build the final config.
    let project = model.project.clone().ok_or_else(|| {
        Error::Diagnostics(vec![Diagnostic::error(
            "build model has no project() declaration; cannot resolve",
        )])
    })?;

    Ok(ResolvedConfig {
        project,
        profile: ResolvedProfile {
            name: profile_name.to_string(),
            target: effective_target,
            opt_level,
            debug_info,
            debug_assertions,
            overflow_checks,
            lto,
            run_binary,
            runner: flattened.runner,
            qemu_memory: flattened.qemu_memory,
            qemu_cores: flattened.qemu_cores,
            qemu_extra_args: flattened.qemu_extra_args.clone().unwrap_or_default(),
            qemu_boot_mode: flattened.qemu_boot_mode,
            test_timeout: flattened.test_timeout,
        },
        options: resolved_options,
        crates: resolved_crates,
        build_dir: project_root.join("build"),
        project_root: project_root.to_path_buf(),
    })
}

/// Local convenience for the per-field merge inside [`flatten_profile`]:
/// for each named `Option<_>` field, copy from `$p` to `$acc` when the
/// source has a value. Intentionally not exported — it folds the
/// repetitive `if p.<f>.is_some() { acc.<f> = p.<f>.clone(); }` chain
/// into a single call site so adding a new profile field touches one
/// list, not a dozen `if` blocks.
macro_rules! inherit_if_set {
    ($acc:ident, $p:ident, $($field:ident),* $(,)?) => {
        $(
            if $p.$field.is_some() {
                $acc.$field = $p.$field.clone();
            }
        )*
    };
}

/// Walk the `inherits` chain depth-first and produce a synthetic
/// [`ProfileDef`] where each `Option` field is filled by the nearest
/// descendant that set it.
///
/// The validate pass already detects inheritance cycles, but we keep a
/// defensive visited-set here so a buggy validator can't OOM the resolver.
fn flatten_profile(
    model: &BuildModel,
    start: Handle<ProfileDef>,
) -> std::result::Result<ProfileDef, Diagnostic> {
    let mut chain: Vec<Handle<ProfileDef>> = Vec::new();
    let mut visited: HashSet<u32> = HashSet::new();
    let mut cur = Some(start);
    while let Some(h) = cur {
        if !visited.insert(h.index()) {
            let name = model
                .profiles
                .get(h)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| format!("#{}", h.index()));
            return Err(Diagnostic::error(format!(
                "profile inheritance cycle detected at '{name}'"
            )));
        }
        chain.push(h);
        let Some(p) = model.profiles.get(h) else {
            break;
        };
        cur = p.inherits;
    }

    // Walk from root → child so child fields overwrite parent fields.
    let mut acc = ProfileDef::default();
    for h in chain.into_iter().rev() {
        let Some(p) = model.profiles.get(h) else {
            continue;
        };
        if !p.name.is_empty() {
            acc.name = p.name.clone();
        }
        inherit_if_set!(
            acc,
            p,
            target,
            opt_level,
            debug_info,
            debug_assertions,
            overflow_checks,
            lto,
            run_binary,
            runner,
            preset,
            qemu_memory,
            qemu_cores,
            qemu_extra_args,
            qemu_boot_mode,
            test_timeout,
        );
        for (k, v) in &p.config {
            acc.config.insert(k.clone(), v.clone());
        }
    }
    Ok(acc)
}

/// Compute the transitive closure of crates reachable from `root` and
/// from every crate built for `target` via `deps` and `artifact_deps`
/// edges. Used when a profile declares a `run_binary` to restrict the
/// build to the crates that are actually needed — preventing wasteful
/// cross-target compilation in multi-profile projects. Crates of the
/// profile's own target are always kept, so a library the run binary
/// does not use is still built and linted.
///
/// Also expands ESP entries: if any source crate in an `EspDef` is reachable,
/// all source crates in that ESP are included (an ESP is an atomic unit that
/// must be assembled whole).
fn reachable_crates(
    model: &BuildModel,
    root: Handle<CrateDef>,
    target: TargetSpec,
) -> BTreeSet<Handle<CrateDef>> {
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    visited.insert(root);
    queue.push_back(root);

    // Target seed: every crate built for the profile's target. A host
    // profile has none; its run binary's closure is the build.
    for (h, krate) in model.crates.iter() {
        let cross = model
            .groups
            .get(krate.group)
            .is_some_and(|g| !matches!(g.target, TargetSpec::Host));
        if cross && target != TargetSpec::Host && krate.target == target && visited.insert(h) {
            queue.push_back(h);
        }
    }

    // Host-test seed: the host test executables (`q.host_tests()`),
    // and the plain libs their doctests link. Nothing depends on them,
    // and they build for the host whatever the profile's target is.
    for (h, krate) in model.crates.iter() {
        if krate.crate_type != CrateType::Test {
            continue;
        }
        if visited.insert(h) {
            queue.push_back(h);
        }
        let lib = krate.cargo.as_ref().and_then(|c| c.doctest_lib.as_deref());
        if let Some(l) = lib.and_then(|n| model.crates.lookup(n))
            && visited.insert(l)
        {
            queue.push_back(l);
        }
    }

    // Build-step seed: every declared build step contributes its runner
    // crate and every `crate_in` input as additional reachability roots.
    // Build steps are model-scope declarations (like images and esps), so
    // they're considered always-active for the purpose of crate selection
    // — quon needs to compile the runner before it can spawn it, and
    // the input crates before it can pass their artifact paths in. Their
    // transitive deps are then walked by the BFS below alongside the
    // run_binary closure. (A future refinement could restrict this to
    // only build steps referenced by the active profile's images, but
    // images are themselves not yet profile-filtered, so the per-profile
    // distinction would buy nothing today.)
    for (_h, step) in model.build_steps.iter() {
        if visited.insert(step.runner) {
            queue.push_back(step.runner);
        }
        for input in step.inputs.values() {
            if let BuildStepInput::Crate { handle: ih } = input
                && visited.insert(*ih)
            {
                queue.push_back(*ih);
            }
        }
    }

    // Image seed: an image is an atomic unit (like an esp), so every
    // crate referenced by any of its top-level entries, partition
    // entries, or raw_writes is pulled in. Without this, a hybrid
    // BIOS/UEFI image referenced by a profile whose run_binary only
    // closes over one half would silently drop the other half's source
    // crates — `compile_image` would later fail with a missing-artifact
    // error far from the configuration mistake. Image source names
    // resolving to esps or build_steps are handled by their own
    // expansions below; we only seed crate names here.
    for (_h, image) in model.images.iter() {
        let mut seed_entries = |entries: &[quon_model::ImageEntry]| {
            for entry in entries {
                if let ImageSource::Crate(name) = &entry.source
                    && let Some(ch) = model.crates.lookup(name)
                    && visited.insert(ch)
                {
                    queue.push_back(ch);
                }
            }
        };
        seed_entries(&image.entries);
        for partition in &image.partitions {
            seed_entries(&partition.entries);
        }
        for rw in &image.raw_writes {
            if let RawSource::Crate { name } = &rw.source
                && let Some(ch) = model.crates.lookup(name)
                && visited.insert(ch)
            {
                queue.push_back(ch);
            }
        }
    }

    while let Some(h) = queue.pop_front() {
        let Some(krate) = model.crates.get(h) else {
            continue;
        };
        // Follow regular deps (--extern edges).
        //
        // Only project-resolved deps contribute crate-arena reachability
        // here. External deps point into `external_deps` and never need
        // a `Handle<CrateDef>` for selection — they are pulled in via the
        // vendor pipeline, not via the crate-graph BFS.
        for dep in krate.deps.values() {
            if let DepResolution::Project(dh) = dep.resolution
                && visited.insert(dh)
            {
                queue.push_back(dh);
            }
        }
        // Follow a manifest unit's build-dependencies, which its build
        // script links.
        let build_deps = krate
            .cargo
            .iter()
            .filter_map(|c| c.build_script.as_ref())
            .flat_map(|s| s.deps.values());
        for dep_name in build_deps {
            if let Some(dh) = model.crates.lookup(dep_name)
                && visited.insert(dh)
            {
                queue.push_back(dh);
            }
        }
        // Follow artifact_deps (ordering-only edges, e.g. bootloader → kernel).
        for dep_name in &krate.artifact_deps {
            if let Some(dh) = model.crates.lookup(dep_name)
                && visited.insert(dh)
            {
                queue.push_back(dh);
            }
        }
    }

    // ESP expansion: if any entry's source crate is reachable, pull in all
    // entries of that ESP. An ESP is assembled as a unit — partial inclusion
    // would produce a broken boot partition.
    for (_h, esp) in model.esps.iter() {
        let mut source_crates = esp.entries.iter().filter_map(|e| e.source.crate_handle());
        if source_crates.clone().any(|h| visited.contains(&h)) {
            // No need to BFS from these — ESP source crates are leaves
            // in the dependency sense (they produce artifacts, not deps).
            // Host-file entries pull in nothing.
            visited.extend(&mut source_crates);
        }
    }

    visited
}

/// Coerce a [`ConfigValue`] into the [`ResolvedValue`] type required by an
/// option, returning `None` (and pushing a diagnostic) on type mismatch.
fn coerce(
    value: &ConfigValue,
    opt: &ConfigOptionDef,
    name: &str,
    diags: &mut Vec<Diagnostic>,
) -> Option<ResolvedValue> {
    coerce_with_label(value, opt, name, diags, "default")
}

fn coerce_with_label(
    value: &ConfigValue,
    opt: &ConfigOptionDef,
    name: &str,
    diags: &mut Vec<Diagnostic>,
    label: &str,
) -> Option<ResolvedValue> {
    match (opt.ty, value) {
        (ConfigType::Bool, ConfigValue::Bool(b)) => Some(ResolvedValue::Bool(*b)),
        (ConfigType::Tristate, ConfigValue::Tristate(t)) => Some(ResolvedValue::Tristate(*t)),
        // Tristate accepts a Bool too: true → Yes, false → No.
        (ConfigType::Tristate, ConfigValue::Bool(b)) => Some(ResolvedValue::Tristate(if *b {
            TristateVal::Yes
        } else {
            TristateVal::No
        })),
        (ConfigType::U32, ConfigValue::U32(n)) => Some(ResolvedValue::U32(*n)),
        (ConfigType::U32, ConfigValue::U64(n)) => {
            if *n <= u32::MAX as u64 {
                Some(ResolvedValue::U32(*n as u32))
            } else {
                diags.push(Diagnostic::error(format!(
                    "{label} sets option '{name}' to {n}, which overflows u32"
                )));
                None
            }
        }
        (ConfigType::U64, ConfigValue::U64(n)) => Some(ResolvedValue::U64(*n)),
        (ConfigType::U64, ConfigValue::U32(n)) => Some(ResolvedValue::U64(*n as u64)),
        (ConfigType::Str, ConfigValue::Str(s)) => Some(ResolvedValue::String(s.clone())),
        (ConfigType::Choice, ConfigValue::Choice(s)) => Some(ResolvedValue::Choice(s.clone())),
        (ConfigType::Choice, ConfigValue::Str(s)) => Some(ResolvedValue::Choice(s.clone())),
        (ConfigType::List, ConfigValue::List(items)) => Some(ResolvedValue::List(items.clone())),
        _ => {
            diags.push(Diagnostic::error(format!(
                "{label} sets option '{name}' to a value of type {} but the option is declared as {}",
                value_type_name(value),
                config_type_name(opt.ty),
            )));
            None
        }
    }
}

fn value_type_name(v: &ConfigValue) -> &'static str {
    match v {
        ConfigValue::Bool(_) => "bool",
        ConfigValue::Tristate(_) => "tristate",
        ConfigValue::U32(_) => "u32",
        ConfigValue::U64(_) => "u64",
        ConfigValue::Str(_) => "str",
        ConfigValue::Choice(_) => "choice",
        ConfigValue::List(_) => "list",
    }
}

fn config_type_name(t: ConfigType) -> &'static str {
    match t {
        ConfigType::Bool => "bool",
        ConfigType::Tristate => "tristate",
        ConfigType::U32 => "u32",
        ConfigType::U64 => "u64",
        ConfigType::Str => "str",
        ConfigType::Choice => "choice",
        ConfigType::List => "list",
        ConfigType::Group => "group",
    }
}

/// If `expr` is (recursively) an `And` of `Ident`s that evaluates false,
/// return the name of the first ident in left-to-right order whose
/// referenced option is off under `lookup`. For any more complex shape
/// (`Or`, `Not`, `Const`, or a mixed `And` containing non-ident
/// children), return `None` — the caller falls back to a generic
/// "expression not satisfied" diagnostic, since there is no single
/// "responsible" dependency to name.
///
/// This exists purely to preserve the legacy-path diagnostic wording
/// for the common case of `.depends_on([A, B])` — it is not load-bearing
/// for correctness.
fn first_unsatisfied_ident<F>(expr: &Expr, lookup: &F) -> Option<String>
where
    F: Fn(&str) -> Option<bool>,
{
    match expr {
        Expr::Ident(name) => {
            if lookup(name).unwrap_or(false) {
                None
            } else {
                Some(name.clone())
            }
        }
        Expr::And(xs) => {
            // Every child must also be Ident-only for the simple
            // diagnostic to apply; any `Or`/`Not`/`Const`/nested `And`
            // rejects the whole traversal so we fall back to the
            // generic message.
            for child in xs {
                match child {
                    Expr::Ident(name) => {
                        if !lookup(name).unwrap_or(false) {
                            return Some(name.clone());
                        }
                    }
                    _ => return None,
                }
            }
            None
        }
        _ => None,
    }
}

/// Returns true when a resolved value should be considered "on" for the
/// purposes of `selects`/`depends_on`. Tristate is treated as a bool:
/// only [`TristateVal::Yes`] counts as on; `Module` and `No` are off.
fn is_on(v: &ResolvedValue) -> bool {
    match v {
        ResolvedValue::Bool(b) => *b,
        ResolvedValue::Tristate(t) => matches!(t, TristateVal::Yes),
        ResolvedValue::U32(n) => *n != 0,
        ResolvedValue::U64(n) => *n != 0,
        ResolvedValue::String(s) => !s.is_empty(),
        ResolvedValue::Choice(s) => !s.is_empty(),
        ResolvedValue::List(items) => !items.is_empty(),
    }
}

/// The "off" value for a given option type. Used by `depends_on` to force
/// an option to its disabled state when a dependency is unsatisfied.
fn off_value_for(opt: &ConfigOptionDef) -> ResolvedValue {
    match opt.ty {
        ConfigType::Bool => ResolvedValue::Bool(false),
        ConfigType::Tristate => ResolvedValue::Tristate(TristateVal::No),
        ConfigType::U32 => ResolvedValue::U32(0),
        ConfigType::U64 => ResolvedValue::U64(0),
        ConfigType::Str => ResolvedValue::String(String::new()),
        ConfigType::Choice => ResolvedValue::Choice(String::new()),
        ConfigType::List => ResolvedValue::List(Vec::new()),
        ConfigType::Group => ResolvedValue::Bool(false),
    }
}

/// The "on" value used by `selects` to force a target option enabled.
/// For non-bool/tristate options, the existing default is left in place
/// where possible — for now we only need a sentinel "on" representation.
fn on_value_for(opt: &ConfigOptionDef) -> ResolvedValue {
    match opt.ty {
        ConfigType::Bool => ResolvedValue::Bool(true),
        ConfigType::Tristate => ResolvedValue::Tristate(TristateVal::Yes),
        // For numeric / string / list / choice types, "selects" pushes a
        // truthy sentinel: 1 for numeric, a single placeholder for string,
        // first choice for choice, single empty-marker for list. The
        // intended use of `selects` is bool/tristate; using it on a
        // numeric option is an unusual pattern, but we still produce a
        // value `is_on` will accept so the fixed point converges.
        ConfigType::U32 => ResolvedValue::U32(1),
        ConfigType::U64 => ResolvedValue::U64(1),
        ConfigType::Str => ResolvedValue::String("y".into()),
        ConfigType::Choice => opt
            .choices
            .as_ref()
            .and_then(|c| c.first())
            .map(|s| ResolvedValue::Choice(s.clone()))
            .unwrap_or_else(|| ResolvedValue::Choice("y".into())),
        ConfigType::List => ResolvedValue::List(vec!["y".into()]),
        ConfigType::Group => ResolvedValue::Bool(true),
    }
}

#[allow(unused)]
fn _crate_handle_marker(_: Handle<CrateDef>, _: Handle<TargetDef>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use quon_config::{Artifact, BuildStep, Crate, Esp, Image, Preset, Profile, Quon, Target, opt};
    use quon_model::{
        BootloaderKind, BuildModel, ConfigValue, ImageFormat, RawBootloaderDef, RawBuildModel,
        RawProfileDef, intern,
    };

    /// Build a `RawBuildModel` in-process with the `Quon` API, for
    /// tests that then change fields the API cannot set.
    fn build_raw<F>(build: F) -> RawBuildModel
    where
        F: FnOnce(&mut Quon),
    {
        let mut q = Quon::new();
        build(&mut q);
        q.into_raw().expect("quon errors")
    }

    fn intern_raw(raw: RawBuildModel) -> BuildModel {
        let (model, diags) = intern::intern(raw);
        assert!(diags.is_empty(), "intern produced diagnostics: {:?}", diags);
        model
    }

    /// Build a `BuildModel` in-process with the `Quon` API and the
    /// quon-model intern pass.
    fn build_model<F>(build: F) -> BuildModel
    where
        F: FnOnce(&mut Quon),
    {
        intern_raw(build_raw(build))
    }

    /// The project, a `default` profile and the spec target `x` that
    /// most tests share.
    fn base(q: &mut Quon) -> Target {
        q.project("t", "1");
        let x = Target::spec("x", "t.json");
        q.default_target(&x);
        q.add(Profile::new("default"));
        x
    }

    fn resolve_default(model: &BuildModel) -> Result<ResolvedConfig> {
        resolve(model, "default", None, Path::new("/tmp/proj"), None)
    }

    /// Helper for the kconfig depends_on tests: build the base model
    /// with `Quon` (project/target/profile), then load and merge a
    /// kconfig file, and resolve the default profile.
    ///
    /// Writing the kconfig to a temp file and loading it drives the
    /// kconfig-parsed `depends_on` expression path end to end.
    fn resolve_kconfig(kconfig: &str) -> Result<ResolvedConfig> {
        let dir = tempfile::tempdir().expect("tempdir");
        let kconfig_path = dir.path().join("options.kconfig");
        std::fs::write(&kconfig_path, kconfig).expect("write kconfig");

        let mut model = build_model(|q| {
            base(q);
        });

        // Load kconfig and merge options/presets directly into the
        // interned BuildModel, mirroring what `script_runner::finalize`
        // does.
        let lowered = crate::kconfig::load_kconfig(&kconfig_path).expect("load kconfig");
        for (name, opt) in lowered.options {
            let _ = model.config_options.insert(name, opt);
        }
        for (name, preset) in lowered.presets {
            model.presets.insert(name, preset);
        }

        resolve(&model, "default", None, dir.path(), None)
    }

    #[test]
    fn resolve_basic_profile() {
        let model = build_model(|q| {
            q.project("t", "1");
            q.default_target(&Target::spec("x", "t.json"));
            q.add(Profile::new("default").opt_level(2).debug_info(true));
        });
        let cfg = resolve_default(&model).expect("resolve");
        assert_eq!(cfg.profile.opt_level, quon_model::OptLevel::Two);
        assert!(cfg.profile.debug_info);
        assert_eq!(
            cfg.profile.target,
            TargetSpec::Cross(model.targets.lookup("x").unwrap())
        );
        assert_eq!(cfg.build_dir, Path::new("/tmp/proj/build"));
    }

    #[test]
    fn debug_assertions_follow_the_profile_then_rustcs_default() {
        let model = build_model(|q| {
            q.project("t", "1");
            q.default_target(&Target::spec("x", "t.json"));
            q.add(Profile::dev().opt_level(1));
            q.add(Profile::new("plain1").opt_level(1));
            q.add(Profile::new("plain0"));
            q.add(Profile::new("checked").opt_level(3).debug_assertions(true));
            q.add(Profile::new("wrapping").overflow_checks(false));
        });
        let flags = |name: &str| {
            let p = resolve(&model, name, None, Path::new("/tmp"), None)
                .expect("resolve")
                .profile;
            (p.debug_assertions, p.overflow_checks)
        };
        assert_eq!(
            flags("dev"),
            (true, true),
            "dev keeps them above opt-level 0"
        );
        assert_eq!(flags("release"), (false, false), "release turns them off");
        assert_eq!(flags("plain1"), (false, false), "unset: rustc's default");
        assert_eq!(flags("plain0"), (true, true), "unset: rustc's default");
        assert_eq!(
            flags("checked"),
            (true, true),
            "overflow follows assertions"
        );
        assert_eq!(flags("wrapping"), (true, false));
    }

    #[test]
    fn resolve_profile_inheritance() {
        let model = build_model(|q| {
            q.project("t", "1");
            q.default_target(&Target::spec("x", "t.json"));
            let base = q.add(Profile::new("base").opt_level(2));
            q.add(Profile::new("dev").inherits(&base).debug_info(true));
        });
        let cfg = resolve(&model, "dev", None, Path::new("/tmp"), None).expect("resolve");
        assert_eq!(
            cfg.profile.opt_level,
            quon_model::OptLevel::Two,
            "should inherit opt_level from base"
        );
        assert!(cfg.profile.debug_info, "child sets debug_info");
        assert_eq!(
            cfg.profile.target,
            TargetSpec::Cross(model.targets.lookup("x").unwrap())
        );
    }

    /// A host profile needs no declared target, and its host crates
    /// resolve as host crates.
    #[test]
    fn resolve_host_profile() {
        let model = build_model(|q| {
            q.project("t", "1");
            let tool = q.add(Crate::bin("tool", "tool"));
            q.run(&tool);
            let dev = q.profile("dev");
            q.add(Profile::new("child").inherits(&dev));
        });
        for name in ["dev", "child"] {
            let cfg = resolve(&model, name, None, Path::new("/tmp"), None).expect("resolve");
            assert_eq!(cfg.profile.target, TargetSpec::Host);
            assert_eq!(cfg.profile.cross_target(), None);
            assert_eq!(cfg.crates.len(), 1);
            assert!(cfg.crates[0].host);
        }
    }

    #[test]
    fn resolve_target_override() {
        // `Quon` registers a target only when something uses it, so
        // the override target `b` goes into the raw model directly.
        let mut raw = build_raw(|q| {
            q.project("t", "1");
            q.default_target(&Target::spec("a", "a.json"));
            q.add(Profile::new("default"));
        });
        raw.targets.insert(
            "b".into(),
            quon_model::TargetDef {
                name: "b".into(),
                spec: "b.json".into(),
                builtin: false,
                build_std: false,
                panic_strategy: None,
                span: None,
            },
        );
        let model = intern_raw(raw);
        let cfg = resolve(&model, "default", Some("b"), Path::new("/tmp"), None).expect("resolve");
        assert_eq!(
            cfg.profile.target,
            TargetSpec::Cross(model.targets.lookup("b").unwrap())
        );
    }

    #[test]
    fn resolve_unknown_profile_error() {
        let model = build_model(|q| {
            base(q);
        });
        let err =
            resolve(&model, "missing", None, Path::new("/tmp"), None).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(v.iter().any(|d| d.message.contains("missing")));
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn resolve_profile_without_target_error() {
        // Every root profile gets a target from `Quon`, so the
        // targetless profile is written into the raw model.
        let mut raw = build_raw(|q| {
            q.project("t", "1");
        });
        raw.profiles.insert(
            "default".into(),
            RawProfileDef {
                name: "default".into(),
                opt_level: Some(quon_model::OptLevel::Zero),
                ..RawProfileDef::default()
            },
        );
        let model = intern_raw(raw);
        let err = resolve_default(&model).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("does not specify a target"))
                );
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn resolve_config_defaults() {
        let model = build_model(|q| {
            base(q);
            q.add(opt::u32("SIZE").default(4096));
            q.add(opt::bool("DEBUG").default(true));
        });
        let cfg = resolve_default(&model).expect("resolve");
        assert_eq!(cfg.options.get("SIZE"), Some(&ResolvedValue::U32(4096)));
        assert_eq!(cfg.options.get("DEBUG"), Some(&ResolvedValue::Bool(true)));
    }

    /// A `default` profile on target `x` that applies preset `big`,
    /// which sets the U32 option `SIZE` to a U64 value.
    fn sized_preset_model() -> BuildModel {
        build_model(|q| {
            q.project("t", "1");
            q.default_target(&Target::spec("x", "t.json"));
            q.add(opt::u32("SIZE").default(4096));
            // `set_named` skips the typed check, so the resolver's
            // coercion of a U64 into a U32 option is still covered.
            let big = q.add(Preset::new("big").set_named("SIZE", ConfigValue::U64(8192)));
            q.add(Profile::new("default").preset(&big));
        })
    }

    #[test]
    fn resolve_config_preset_override() {
        let model = sized_preset_model();
        let cfg = resolve_default(&model).expect("resolve");
        // Note: preset.set with U64 but option declared U32; coercion narrows it.
        assert_eq!(cfg.options.get("SIZE"), Some(&ResolvedValue::U32(8192)));
    }

    #[test]
    fn resolve_config_external_override() {
        let model = sized_preset_model();
        let mut overrides = BTreeMap::new();
        overrides.insert("SIZE".to_string(), ConfigValue::U32(16384));
        let cfg =
            resolve(&model, "default", None, Path::new("/tmp"), Some(&overrides)).expect("resolve");
        assert_eq!(cfg.options.get("SIZE"), Some(&ResolvedValue::U32(16384)));
    }

    #[test]
    fn resolve_config_range_violation() {
        let model = build_model(|q| {
            base(q);
            q.add(opt::u32("SIZE").default(999999).range(0..=65536));
        });
        let err = resolve_default(&model).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("outside declared range")),
                    "expected range diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn resolve_config_depends_on_expr_and_satisfied() {
        // A && B with both A and B on → X stays at its default (true).
        let cfg = resolve_kconfig(
            r#"
            config A: bool { default = true }
            config B: bool { default = true }
            config X: bool { default = true depends_on = A && B }
            "#,
        )
        .expect("resolve");
        assert_eq!(cfg.options.get("X"), Some(&ResolvedValue::Bool(true)));
    }

    #[test]
    fn resolve_config_depends_on_expr_and_unsatisfied() {
        // A && B but B is off → X must be forced off.
        let cfg = resolve_kconfig(
            r#"
            config A: bool { default = true }
            config B: bool { default = false }
            config X: bool { default = true depends_on = A && B }
            "#,
        );
        // The resolver pushes a diagnostic on the disable, so the
        // result is Err with a Diagnostics payload — not a hard
        // failure, but the resolver still flips X off in the model.
        // The test just confirms the diagnostic appears.
        //
        // For a plain `And(Ident, Ident)` expression the collapsed
        // resolver walks the tree to name the first unsatisfied ident
        // (B), matching the legacy dep-name wording. The generic
        // "expression is not satisfied" phrasing is reserved for shapes
        // `first_unsatisfied_ident` can't simplify (`Or`, `Not`, mixed).
        match cfg {
            Err(Error::Diagnostics(v)) => {
                assert!(
                    v.iter().any(|d| d.message.contains("dependency 'B'")),
                    "expected diagnostic naming B as the unsatisfied dep, got: {v:?}"
                );
            }
            other => panic!("expected Diagnostics about depends_on expression, got {other:?}"),
        }
    }

    #[test]
    fn resolve_config_depends_on_expr_or_unsatisfied_uses_generic_message() {
        // For `A || B` with both A and B off, the expression doesn't
        // reduce to a single responsible dep, so the resolver falls
        // back to the generic "expression is not satisfied" message.
        let cfg = resolve_kconfig(
            r#"
            config A: bool { default = false }
            config B: bool { default = false }
            config X: bool { default = true depends_on = A || B }
            "#,
        );
        match cfg {
            Err(Error::Diagnostics(v)) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("'depends_on' expression")),
                    "expected generic expression-not-satisfied diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn resolve_config_depends_on_expr_or_one_satisfied() {
        // A || B with only A on → X stays satisfied. This is the case
        // where flatten-based eval would WRONGLY disable X (because B
        // is off and the flat form treats it as required).
        let cfg = resolve_kconfig(
            r#"
            config A: bool { default = true }
            config B: bool { default = false }
            config X: bool { default = true depends_on = A || B }
            "#,
        )
        .expect("resolve");
        assert_eq!(cfg.options.get("X"), Some(&ResolvedValue::Bool(true)));
    }

    #[test]
    fn resolve_config_depends_on_expr_not_inverts() {
        // !DISABLED with DISABLED off → expression is true → X enabled.
        let cfg = resolve_kconfig(
            r#"
            config DISABLED: bool { default = false }
            config X: bool { default = true depends_on = !DISABLED }
            "#,
        )
        .expect("resolve");
        assert_eq!(cfg.options.get("X"), Some(&ResolvedValue::Bool(true)));
    }

    #[test]
    fn resolve_config_selects() {
        let model = build_model(|q| {
            base(q);
            let b = q.add(opt::bool("B").default(false));
            q.add(opt::bool("A").default(true).selects(&b));
        });
        let cfg = resolve_default(&model).expect("resolve");
        assert_eq!(cfg.options.get("A"), Some(&ResolvedValue::Bool(true)));
        assert_eq!(
            cfg.options.get("B"),
            Some(&ResolvedValue::Bool(true)),
            "B should be forced on by A's selects"
        );
    }

    #[test]
    fn resolve_interpolation() {
        let model = build_model(|q| {
            base(q);
            q.add(opt::str("NAME").default("foo"));
            q.add(opt::str("MSG").default("hello ${NAME}"));
        });
        let cfg = resolve_default(&model).expect("resolve");
        assert_eq!(
            cfg.options.get("MSG"),
            Some(&ResolvedValue::String("hello foo".into()))
        );
    }

    #[test]
    fn resolve_interpolation_cycle_is_error() {
        let model = build_model(|q| {
            base(q);
            q.add(opt::str("A").default("${B}"));
            q.add(opt::str("B").default("${A}"));
        });
        let err = resolve_default(&model).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter().any(|d| d.message.contains("cycle")),
                    "expected cycle diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn resolve_crates_list() {
        let model = build_model(|q| {
            base(q);
            q.add(Crate::lib("kfoo", "crates/kfoo"));
            q.add(Crate::lib("hbar", "crates/hbar").target(&Target::host()));
        });
        let cfg = resolve_default(&model).expect("resolve");
        let names: Vec<&str> = cfg
            .crates
            .iter()
            .map(|c| model.crates.get(c.handle).unwrap().name.as_str())
            .collect();
        assert!(
            names.contains(&"kfoo"),
            "kernel crate present, got {names:?}"
        );
        assert!(names.contains(&"hbar"), "host crate present, got {names:?}");
        let host_entry = cfg
            .crates
            .iter()
            .find(|c| model.crates.get(c.handle).unwrap().name == "hbar")
            .unwrap();
        assert!(host_entry.host, "host crate flagged correctly");
    }

    // -----------------------------------------------------------------------
    // Per-profile crate filtering (reachable_crates)
    // -----------------------------------------------------------------------

    /// Helper: resolve a named profile and return the set of crate names.
    fn crate_names_for_profile(model: &BuildModel, profile: &str) -> Vec<String> {
        let cfg = resolve(model, profile, None, Path::new("/tmp/proj"), None).expect("resolve");
        cfg.crates
            .iter()
            .map(|c| model.crates.get(c.handle).unwrap().name.clone())
            .collect()
    }

    #[test]
    fn filter_no_run_binary_includes_all() {
        let model = build_model(|q| {
            base(q);
            let y = Target::spec("y", "y.json");
            q.add(Crate::lib("crate_a", "crates/a"));
            q.add(Crate::lib("crate_b", "crates/b").target(&y));
        });
        let names = crate_names_for_profile(&model, "default");
        assert!(names.contains(&"crate_a".into()), "got {names:?}");
        assert!(
            names.contains(&"crate_b".into()),
            "no run_binary → all crates included, got {names:?}"
        );
    }

    #[test]
    fn filter_run_binary_restricts_to_reachable() {
        let model = build_model(|q| {
            q.project("t", "1");
            let x = Target::spec("x", "t.json");
            let y = Target::spec("y", "y.json");
            let a = q.add(Crate::bin("crate_a", "crates/a").target(&x));
            q.add(Crate::bin("crate_b", "crates/b").target(&y));
            q.run(&a);
            q.add(Profile::new("default"));
        });
        let names = crate_names_for_profile(&model, "default");
        assert!(names.contains(&"crate_a".into()), "run_binary is reachable");
        assert!(
            !names.contains(&"crate_b".into()),
            "crate_b is unreachable from crate_a, got {names:?}"
        );
    }

    #[test]
    fn filter_run_binary_keeps_crates_of_the_profile_target() {
        // A library on the profile's own target that the run binary
        // does not use still builds: only other targets' crates are
        // filtered. Its host proc macro comes with it.
        let model = build_model(|q| {
            q.project("t", "1");
            let x = Target::spec("x", "t.json");
            let y = Target::spec("y", "y.json");
            let derive =
                q.add(Crate::proc_macro("lib_derive", "crates/derive").target(&Target::host()));
            let kernel = q.add(Crate::bin("kernel", "crates/kernel").target(&x));
            q.add(Crate::lib("lib", "crates/lib").target(&x).dep(&derive));
            q.add(Crate::lib("other", "crates/other").target(&y));
            q.run(&kernel);
            q.add(Profile::new("default"));
        });
        let names = crate_names_for_profile(&model, "default");
        assert!(names.contains(&"kernel".into()), "got {names:?}");
        assert!(
            names.contains(&"lib".into()),
            "same-target crate unused by the run binary, got {names:?}"
        );
        assert!(
            names.contains(&"lib_derive".into()),
            "host dep of a same-target crate, got {names:?}"
        );
        assert!(
            !names.contains(&"other".into()),
            "crate on another target, got {names:?}"
        );
    }

    /// A kernel on target `x` and a bootloader on `y` that embeds it
    /// through `artifact_env`, with the bootloader as the run binary.
    fn kernel_and_bootloader(q: &mut Quon) -> Artifact {
        q.project("t", "1");
        let x = Target::spec("x", "t.json");
        let y = Target::spec("y", "y.json");
        let kernel = q.add(Crate::bin("kernel", "crates/kernel").target(&x));
        let boot = q.add(
            Crate::bin("bootloader", "crates/boot")
                .target(&y)
                .artifact_env("KERNEL_PATH", &kernel),
        );
        q.run(&boot);
        q.add(Profile::new("default"));
        boot
    }

    #[test]
    fn filter_artifact_deps_cross_target_included() {
        // Bootloader on target Y depends on kernel on target X via artifact_env.
        let model = build_model(|q| {
            kernel_and_bootloader(q);
        });
        let names = crate_names_for_profile(&model, "default");
        assert!(
            names.contains(&"bootloader".into()),
            "run_binary itself, got {names:?}"
        );
        assert!(
            names.contains(&"kernel".into()),
            "kernel reachable via artifact_deps, got {names:?}"
        );
    }

    #[test]
    fn filter_deps_host_crate_included() {
        // Cross crate depends on a host proc-macro — both should be included.
        let model = build_model(|q| {
            q.project("t", "1");
            let x = Target::spec("x", "t.json");
            let derive = q.add(Crate::proc_macro("my_derive", "crates/derive"));
            let kernel = q.add(
                Crate::bin("kernel", "crates/kernel")
                    .target(&x)
                    .dep(&derive),
            );
            q.run(&kernel);
            q.add(Profile::new("default"));
        });
        let names = crate_names_for_profile(&model, "default");
        assert!(names.contains(&"kernel".into()), "got {names:?}");
        assert!(
            names.contains(&"my_derive".into()),
            "host crate reachable via deps, got {names:?}"
        );
    }

    #[test]
    fn filter_two_profiles_disjoint_sets() {
        // Two profiles with different run_binary should produce disjoint
        // crate sets. `Quon` has one run entry per project, so the two
        // profiles go into the raw model directly.
        let mut raw = build_raw(|q| {
            q.project("t", "1");
            let x = Target::spec("x", "t.json");
            let y = Target::spec("y", "y.json");
            q.add(Crate::bin("crate_x", "crates/x").target(&x));
            q.add(Crate::bin("crate_y", "crates/y").target(&y));
            q.default_target(&x);
        });
        for (name, target, bin) in [("prof_x", "x", "crate_x"), ("prof_y", "y", "crate_y")] {
            raw.profiles.insert(
                name.into(),
                RawProfileDef {
                    name: name.into(),
                    target: Some(target.into()),
                    run_binary: Some(bin.into()),
                    ..RawProfileDef::default()
                },
            );
        }
        let model = intern_raw(raw);
        let names_x = crate_names_for_profile(&model, "prof_x");
        let names_y = crate_names_for_profile(&model, "prof_y");
        assert_eq!(names_x, vec!["crate_x"], "prof_x only builds crate_x");
        assert_eq!(names_y, vec!["crate_y"], "prof_y only builds crate_y");
    }

    #[test]
    fn filter_esp_expansion() {
        // ESP source crate is pulled in even if not directly reachable,
        // as long as another entry in the same ESP is reachable.
        let model = build_model(|q| {
            let boot = kernel_and_bootloader(q);
            q.add(Esp::new("default").add(&boot, "EFI/BOOT/BOOTX64.EFI"));
        });
        let names = crate_names_for_profile(&model, "default");
        assert!(
            names.contains(&"bootloader".into()),
            "ESP source crate included, got {names:?}"
        );
    }

    #[test]
    fn filter_image_source_crate_pulled_in() {
        // An image references a crate that the run_binary closure does
        // not reach. Without the image-seed expansion, that crate would
        // be silently dropped and `compile_image` would later fail with
        // a missing-artifact error far from the configuration.
        let model = build_model(|q| {
            q.project("t", "1");
            let x = Target::spec("x", "t.json");
            let y = Target::spec("y", "y.json");
            let a = q.add(Crate::bin("crate_a", "crates/a").target(&x));
            let b = q.add(Crate::bin("crate_b", "crates/b").target(&y));
            q.run(&a);
            q.add(Profile::new("default"));
            q.add(Image::new("disk").size_mb(1).raw_write(0, &b, None));
        });
        let names = crate_names_for_profile(&model, "default");
        assert!(
            names.contains(&"crate_b".into()),
            "image-referenced crate_b should be in the reachable set, got {names:?}"
        );
    }

    #[test]
    fn filter_build_step_runner_and_inputs_pulled_in() {
        // A build step's tool crate (a host bin) and its crate inputs
        // must be in the reachable set even when the active profile's
        // run_binary closure doesn't reach them — quon needs to compile
        // the tool before spawning it and the inputs before passing
        // their artifact paths in.
        let model = build_model(|q| {
            q.project("t", "1");
            let x = Target::spec("x", "t.json");
            let a = q.add(Crate::bin("crate_a", "crates/a").target(&x));
            let tool = q.add(Crate::bin("runner", "tools/runner").target(&Target::host()));
            let input = q.add(Crate::bin("input_crate", "crates/inp").target(&x));
            q.add(
                BuildStep::new("step", "out.bin")
                    .tool(&tool)
                    .input("INPUT", &input),
            );
            q.run(&a);
            q.add(Profile::new("default"));
        });
        let names = crate_names_for_profile(&model, "default");
        assert!(
            names.contains(&"runner".into()),
            "build_step runner should be reachable, got {names:?}"
        );
        assert!(
            names.contains(&"input_crate".into()),
            "build_step crate_in input should be reachable, got {names:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Intern tests for BootloaderDef and ImageDef
    // -----------------------------------------------------------------------

    #[test]
    fn raw_bootloader_interns_typed_fields() {
        // `Quon` no longer declares a bootloader, but the model still
        // carries one, so the raw form is written directly.
        let mut raw = build_raw(|q| {
            let x = base(q);
            q.add(Crate::lib("boot", "crates/boot").target(&x));
        });
        raw.bootloader = Some(RawBootloaderDef {
            kind: BootloaderKind::Uefi,
            protocol: Some("gop".into()),
            entry_crate: Some("boot".into()),
            config_file: None,
            extras: BTreeMap::new(),
            span: None,
        });
        let model = intern_raw(raw);
        let bl = model
            .bootloader
            .as_ref()
            .expect("bootloader populated by intern");
        assert_eq!(bl.kind, BootloaderKind::Uefi);
        let entry = bl
            .entry_crate
            .and_then(|h| model.crates.get(h))
            .map(|c| c.name.as_str());
        assert_eq!(entry, Some("boot"));
        assert_eq!(bl.protocol.as_deref(), Some("gop"));
    }

    #[test]
    fn image_builder_populates_model() {
        let model = build_model(|q| {
            base(q);
            let boot = q.add(Crate::bin("bootloader", "crates/boot"));
            let esp = q.add(Esp::new("default"));
            q.add(
                Image::new("disk")
                    .format(ImageFormat::Fat32)
                    .size_mb(64)
                    .add(&boot, "EFI/BOOT/BOOTX64.EFI")
                    .add("splash.bmp", "boot/splash.bmp")
                    .add(&esp, "/"),
            );
        });
        let h = model.images.lookup("disk").expect("image must exist");
        let img = model.images.get(h).unwrap();
        assert_eq!(img.format, Some(ImageFormat::Fat32));
        assert_eq!(img.size_mb, Some(64));
        assert_eq!(img.entries.len(), 3);
    }
}
