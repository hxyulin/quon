// Fixture for the negative RawBinary integration test.
//
// Identical to raw-binary-minimal except the project name and the
// linker script, which contains OUTPUT_FORMAT(binary) — a directive
// that collides with gluon's post-link objcopy step and must produce
// a specific diagnostic pointing the user at the offending directive.

use gluon_config::{Config, CrateType, Result};

fn main() -> Result<()> {
    let mut cfg = Config::new();

    cfg.project("raw-binary-bad-linker", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true);

    cfg.group("boot")
        .target(target)
        .edition("2021")
        .add("boot", "crates/boot")
        .crate_type(CrateType::RawBinary)
        .root("src/main.rs")
        .linker_script("crates/boot/boot.ld");

    cfg.emit()
}
