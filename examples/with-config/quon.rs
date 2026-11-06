// examples/with-config/quon.rs
//
// Project using Quon's Kconfig-style configuration system.
//
// Config options become compile-time constants in an auto-generated
// `<project>_config` crate. They can be declared inline (as below)
// or loaded from a `.kconfig` file with `q.include_kconfig(path)`.
//
//     quon build
//     quon build --preset verbose
//
// The kernel package comes from the Cargo workspace in this
// directory; see crates/kernel/src/main.rs for the config crate in
// use.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("configurable_kernel", "0.1.0");
    q.cargo_workspace(".");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    // ── Config options ────────────────────────────────────────────
    // Each option becomes a constant in the generated
    // `configurable_kernel_config` crate. `opt::bool`, `opt::u32`,
    // `opt::str` and the rest set its type. `depends_on` takes a
    // condition built from option handles with `&`, `|` and `!`.
    let log_enabled = q.add(
        opt::bool("LOG_ENABLED")
            .default(true)
            .help("Enable runtime logging subsystem"),
    );

    let log_level = q.add(
        opt::u32("LOG_LEVEL")
            .default(3)
            .range(0..=5)
            .depends_on(&log_enabled)
            .help("Verbosity: 0 = off, 5 = trace"),
    );

    let debug = q.add(
        opt::bool("DEBUG")
            .default(false)
            .help("Enable kernel debug assertions and extra checks"),
    );

    q.add(
        opt::bool("FANCY_FEATURE")
            .default(true)
            .depends_on(&log_enabled & !&debug)
            .help("A feature that requires logging but is disabled in debug mode"),
    );

    // ── Presets ───────────────────────────────────────────────────
    q.add(Preset::new("verbose").set(&log_level, 5).set(&debug, true));
    q.add(Preset::new("quiet").set(&log_enabled, false));

    // `link_config` links the generated config crate into the kernel.
    // Its workspace dependencies do not link it.
    q.add(
        Package::new("kernel")
            .link_config()
            .linker_script("crates/kernel/kernel.ld"),
    );

    // ── Using config in Rust ──────────────────────────────────────
    // The generated config crate is named `<project>_config`, so:
    //
    //     use configurable_kernel_config::*;
    //
    //     if LOG_ENABLED { log::set_level(LOG_LEVEL); }
    //
    // Alternatively, declare options in a `.kconfig` file:
    //
    //     q.include_kconfig("options.kconfig");
    //
    // The .kconfig format supports menus, `depends on`, `source`,
    // and the full `||`/`&&`/`!` expression grammar.

    Ok(())
}
