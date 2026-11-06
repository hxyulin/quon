// K9 fixture: a tiny project that loads its config from a .kconfig file
// instead of declaring options inline. The integration test runs this
// alongside `kconfig-builder-equiv/` and asserts that both produce the
// same resolved option set, proving the loader is on parity with the
// builder API.
//
// The kconfig file additionally exercises features tested separately
// via direct resolved-config inspection: `||`/`!` in `depends_on`,
// nested menus, `source` and preset overrides.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("kconfig-minimal", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    // The parity test resolves this profile by name.
    q.add(Profile::new("default").opt_level(0).debug_info(true));

    q.include_kconfig("./options.kconfig");

    Ok(())
}
