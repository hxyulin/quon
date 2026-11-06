// K9 fixture: a tiny project that loads its config from a .kconfig file
// instead of declaring options inline. The integration test runs this
// alongside `kconfig-rhai-equiv/` and asserts that both produce the
// same resolved option set, proving the loader is on-parity with the
// existing Rhai builder for the simple AND-of-symbols subset.
//
// The kconfig file additionally exercises features the Rhai builder
// cannot express (`||`/`!` in `depends_on`, nested menus, `source`,
// preset overrides), which are tested separately via direct
// resolved-config inspection.

use gluon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("kconfig-minimal", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("default")
        .target(target)
        .opt_level(0)
        .debug_info(true);

    cfg.include_kconfig("./options.kconfig")?;

    Ok(())
}
