// K9 fixture: the builder-API equivalent of `kconfig-minimal/options.kconfig`.
//
// FANCY_FEATURE uses an `&&`/`!` condition, built here from option
// handles with `&` and `!`, so this fixture declares the same shape
// the `.kconfig` file does. The K9 parity test compares every option
// across both fixtures; if the resolver produced different values for
// FANCY_FEATURE between the two forms, the round-trip test would
// notice.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("kconfig-minimal", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    // The parity test resolves this profile by name.
    q.add(Profile::new("default").opt_level(0).debug_info(true));

    let log_enabled = q.add(
        opt::bool("LOG_ENABLED")
            .default(true)
            .help("Enable runtime logging"),
    );
    let log_level = q.add(
        opt::u32("LOG_LEVEL")
            .default(3)
            .range(0..=5)
            .depends_on(&log_enabled),
    );
    let debug = q.add(opt::bool("DEBUG").default(false));
    q.add(
        opt::bool("FANCY_FEATURE")
            .default(true)
            .depends_on(&log_enabled & !&debug),
    );

    q.add(Preset::new("verbose").set(&log_level, 5).set(&debug, true));

    Ok(())
}
