// examples/with-config/quon.rs
//
// Project using Quon's Kconfig-style configuration system.
//
// Config options become compile-time constants in an auto-generated
// `<project>_config` crate.  They can be declared inline (as below)
// or loaded from a `.kconfig` file via `cfg.include_kconfig(path)?`.
//
//     quon build
//     quon build --preset verbose
//
// The kernel package comes from the Cargo workspace in this
// directory; see crates/kernel/src/main.rs for the config crate in
// use.

use quon_config::{Config, ConfigValue, Expr, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("configurable_kernel", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");
    cfg.cargo_workspace(".");

    cfg.profile("default")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .run_binary("kernel");

    // ── Config options ────────────────────────────────────────────
    // Typed configuration options. Each becomes a constant in the
    // auto-generated `configurable_kernel_config` crate.
    //
    // Available types:
    //   config_bool(name)   — boolean constant
    //   config_u32(name)    — unsigned 32-bit integer constant
    //   config_str(name)    — string constant
    //
    // Chained methods:
    //   .default_value(v)   — value used when nothing overrides it
    //   .help("...")        — human-readable description
    //   .range(lo, hi)      — valid range (integers only)
    //   .depends_on(e)      — boolean expression that gates the
    //                         option. `Expr::Ident("X")` for a
    //                         single dependency, `Expr::And/Or/Not`
    //                         for compound clauses. The engine's
    //                         string-form parser is not exposed in
    //                         the Rust DSL — declare options in a
    //                         `.kconfig` file via `include_kconfig`
    //                         if you want the string-form grammar.

    cfg.config_bool("LOG_ENABLED")
        .default_value(ConfigValue::Bool(true))
        .help("Enable runtime logging subsystem");

    cfg.config_u32("LOG_LEVEL")
        .default_value(ConfigValue::U32(3))
        .range(0, 5)
        .depends_on(Expr::Ident("LOG_ENABLED".into()))
        .help("Verbosity: 0 = off, 5 = trace");

    cfg.config_bool("DEBUG")
        .default_value(ConfigValue::Bool(false))
        .help("Enable kernel debug assertions and extra checks");

    cfg.config_bool("FANCY_FEATURE")
        .default_value(ConfigValue::Bool(true))
        .depends_on(Expr::And(vec![
            Expr::Ident("LOG_ENABLED".into()),
            Expr::Not(Box::new(Expr::Ident("DEBUG".into()))),
        ]))
        .help("A feature that requires logging but is disabled in debug mode");

    // ── Presets ───────────────────────────────────────────────────
    cfg.preset("verbose")
        .set("LOG_LEVEL", ConfigValue::U32(5))
        .set("DEBUG", ConfigValue::Bool(true));

    cfg.preset("quiet")
        .set("LOG_ENABLED", ConfigValue::Bool(false));

    // ── Group + package ───────────────────────────────────────────
    // `config(true)` links the generated config crate into the
    // group's packages. Their workspace dependencies do not link it.
    cfg.group("kernel")
        .target(target)
        .config(true)
        .package("kernel")
        .linker_script("crates/kernel/kernel.ld");

    // ── Using config in Rust ──────────────────────────────────────
    // The generated config crate is named `<project>_config`, so:
    //
    //     use configurable_kernel_config::*;
    //
    //     if LOG_ENABLED { log::set_level(LOG_LEVEL); }
    //
    // Alternatively, declare options in a `.kconfig` file:
    //
    //     cfg.include_kconfig("./options.kconfig")?;
    //
    // The .kconfig format supports menus, `depends on`, `source`,
    // and the full `||`/`&&`/`!` expression grammar.

    Ok(())
}
