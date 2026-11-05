//! Post-resolution types consumed by the scheduler and compile pipeline.
//!
//! After the engine evaluates `gluon.rs` and the user selects a profile,
//! [`crate::build_model::BuildModel`] is collapsed into a
//! [`ResolvedConfig`]: profile inheritance is applied, config options are
//! resolved to final typed values, and crate/target bindings are frozen.
//! Everything downstream of resolution works with these types, never with
//! the raw build model directly.

use crate::build_model::{BootMode, CrateDef, ProjectDef, TargetDef};
use crate::handle::Handle;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A value for a config option resolved to its final typed form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResolvedValue {
    Bool(bool),
    Tristate(crate::kconfig::TristateVal),
    U32(u32),
    U64(u64),
    String(String),
    Choice(String),
    List(Vec<String>),
}

/// A profile after inheritance has been applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedProfile {
    pub name: String,
    pub target: Handle<TargetDef>,
    pub opt_level: u8,
    pub debug_info: bool,
    pub lto: Option<String>,
    pub boot_binary: Option<Handle<CrateDef>>,
    /// Profile-level QEMU memory override (MiB). Takes precedence over
    /// `QemuDef::memory_mb` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu_memory: Option<u32>,
    /// Profile-level QEMU core count override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu_cores: Option<u32>,
    /// Extra QEMU args contributed by the profile; appended after
    /// `QemuDef::extra_args`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub qemu_extra_args: Vec<String>,
    /// Profile-level boot mode. Wins over `QemuDef::boot_mode` when set.
    /// CLI flags (`--uefi` / `--direct` / `--bios`) win over both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu_boot_mode: Option<BootMode>,
    /// Profile-level timeout (seconds) for `gluon run`/`test` invocations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_timeout: Option<u32>,
}

/// A crate selected for inclusion in the build, with resolved target binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedCrateRef {
    pub handle: Handle<CrateDef>,
    /// Always equals the crate's own `target_handle` after resolution; carried
    /// here so the scheduler can avoid the indirection. For host crates this
    /// is a placeholder (the resolved target of the active profile) and the
    /// `host` flag should be consulted instead.
    pub target: Handle<TargetDef>,
    /// True when this crate's group targets the literal `"host"` triple.
    /// Host crates are built for the build machine and are not subject to
    /// the profile's target.
    #[serde(default)]
    pub host: bool,
}

impl ResolvedProfile {
    /// Inherit unset fields from `parent`. "Unset" means `None` for
    /// `Option`s and empty for collections. Fields the child has
    /// already set keep their values.
    ///
    /// `target` is mandatory on both sides — never inherited here
    /// (target inheritance is handled at the chain-walk in
    /// `config/resolve.rs::flatten_profile` because it must produce
    /// a single concrete handle).
    ///
    /// No production caller today: `flatten_profile` operates on
    /// pre-defaults `ProfileDef` via the `inherit_if_set!` macro.
    /// Retained for prospective consumers that hold already-resolved
    /// profiles (e.g. a future `gluon profile show <name>` or LSP
    /// hover that needs to flatten an inherited chain without
    /// re-running resolve). Drop this method if no caller materialises.
    pub fn merge_inherited(&mut self, parent: &Self) {
        if self.lto.is_none() {
            self.lto = parent.lto.clone();
        }
        if self.boot_binary.is_none() {
            self.boot_binary = parent.boot_binary;
        }
        if self.qemu_memory.is_none() {
            self.qemu_memory = parent.qemu_memory;
        }
        if self.qemu_cores.is_none() {
            self.qemu_cores = parent.qemu_cores;
        }
        if self.qemu_extra_args.is_empty() {
            self.qemu_extra_args = parent.qemu_extra_args.clone();
        }
        if self.qemu_boot_mode.is_none() {
            self.qemu_boot_mode = parent.qemu_boot_mode;
        }
        if self.test_timeout.is_none() {
            self.test_timeout = parent.test_timeout;
        }
        // opt_level and debug_info have concrete defaults from
        // resolve_with_defaults — never inherited via this helper.
    }
}

/// A fully-resolved build configuration, consumed by scheduler/compile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedConfig {
    pub project: ProjectDef,
    pub profile: ResolvedProfile,
    pub options: BTreeMap<String, ResolvedValue>,
    pub crates: Vec<ResolvedCrateRef>,
    pub build_dir: PathBuf,
    /// Absolute path to the project root (the directory containing
    /// `gluon.rs`). Used by `compile_crate` to resolve `CrateDef::path`
    /// and `CrateDef::linker_script` against the project tree.
    pub project_root: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::Handle;

    #[test]
    fn merge_inherited_fills_none_slots_only() {
        let parent = ResolvedProfile {
            name: "parent".into(),
            target: Handle::new(0),
            opt_level: 2,
            debug_info: false,
            lto: Some("fat".into()),
            boot_binary: None,
            qemu_memory: Some(512),
            qemu_cores: Some(4),
            qemu_extra_args: vec!["-cpu".into(), "max".into()],
            qemu_boot_mode: Some(crate::build_model::BootMode::Uefi),
            test_timeout: Some(60),
        };
        let mut child = ResolvedProfile {
            name: "child".into(),
            target: Handle::new(0),
            opt_level: 0, // child overrides
            debug_info: true,
            lto: None, // child inherits
            boot_binary: None,
            qemu_memory: None, // child inherits
            qemu_cores: None,
            qemu_extra_args: vec![], // empty inherits
            qemu_boot_mode: None,
            test_timeout: None,
        };
        child.merge_inherited(&parent);
        assert_eq!(
            child.opt_level, 0,
            "child's value preserved (already non-default)"
        );
        assert!(child.debug_info, "child's value preserved");
        assert_eq!(
            child.lto,
            Some("fat".to_string()),
            "lto inherited from parent"
        );
        assert_eq!(child.qemu_memory, Some(512), "inherited from parent");
        assert_eq!(child.qemu_cores, Some(4), "inherited");
        assert_eq!(
            child.qemu_extra_args,
            vec!["-cpu".to_string(), "max".to_string()],
            "empty Vec inherited"
        );
        assert_eq!(
            child.qemu_boot_mode,
            Some(crate::build_model::BootMode::Uefi),
            "inherited"
        );
        assert_eq!(child.test_timeout, Some(60), "inherited");
    }
}
