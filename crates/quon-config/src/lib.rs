//! The Rust API for `quon.rs` build scripts.
//!
//! A `quon.rs` defines one function, which fills in a [`Quon`]:
//!
//! ```
//! use quon_config::prelude::*;
//!
//! pub fn config(q: &mut Quon) -> Result<()> {
//!     q.project("my-os", "0.1.0");
//!     q.cargo_workspace(".");
//!
//!     let none = Target::builtin("x86_64-unknown-none");
//!     let uefi = Target::builtin("x86_64-unknown-uefi");
//!
//!     let kernel = q.add(Package::new("kernel").target(&none).linker_script("kernel.ld"));
//!     let boot = q.add(Package::new("bootloader").target(&uefi).artifact_env("KERNEL_PATH", &kernel));
//!     q.add(Esp::new("boot").add(&boot, "EFI/BOOT/BOOTX64.EFI"));
//!
//!     let qemu = q.add(Qemu::new("qemu-system-x86_64").machine("q35").boot(BootMode::Uefi));
//!     q.run(&boot).runner(&qemu);
//!     q.add(Profile::release().lto(Lto::Thin));
//!     Ok(())
//! }
//! # let mut q = Quon::new();
//! # config(&mut q).unwrap();
//! # q.into_raw().unwrap();
//! ```
//!
//! [`Quon::add`] declares an item (a package, a runner, an ESP) and
//! returns a handle; other declarations take that handle, so they can
//! only refer to something that exists. Methods on [`Quon`] set what a
//! project has one of.
//!
//! Every project has a `dev` and a `release` profile, and profiles
//! build for the target of what [`Quon::run`] starts.

mod artifact;
mod error;
mod files;
mod krate;
pub mod opt;
mod profile;
mod quon;
mod runner;
mod target;
mod tests;

pub use artifact::{Artifact, Source};
pub use error::{ConfigError, Result};
pub use files::{BuildStep, Download, Esp, Image, Partition};
pub use krate::{Crate, Dep, Dependency, Edition, Package};
pub use opt::{Cond, OptRef, Preset, PresetRef};
pub use profile::{IntoOptLevel, Lto, Profile, ProfileRef};
pub use quon::{CargoWorkspace, Declare, Quon, Run};
pub use runner::{Command, Plugin, Qemu, Runner};
pub use target::Target;
pub use tests::{BootTest, HostTests};

pub use quon_model::{
    Binding, BootMode, ConfigValue, CrateType, DisplayMode, ImageFormat, OptLevel, PartitionTable,
    RawBuildModel, SerialMode as Serial, TristateVal,
};
/// For plugin settings that are arrays or objects:
/// `.setting("probe", quon_config::serde_json::json!({"speed": 4000}))`.
pub use serde_json;

/// Everything a `quon.rs` uses: `use quon_config::prelude::*;`.
pub mod prelude {
    pub use crate::opt;
    pub use crate::{
        Artifact, Binding, BootMode, BootTest, BuildStep, Command, Cond, ConfigValue, Crate,
        CrateType, Declare, Dep, Dependency, DisplayMode, Download, Edition, Esp, Image,
        ImageFormat, Lto, OptLevel, Package, Partition, PartitionTable, Plugin, Preset, Profile,
        Qemu, Quon, Result, Runner, Serial, Target, TristateVal,
    };
}
