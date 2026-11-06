// K9 fixture: the pure-Rhai equivalent of `kconfig-minimal/options.kconfig`.
//
// Including FANCY_FEATURE — which uses the full `&&`/`!` expression
// grammar — is the whole point of the `.depends_on_expr(...)` Rhai
// method: it reuses the `.kconfig` expression parser so this fixture
// can declare the same shape the `.kconfig` file does without a second
// grammar. The K9 parity test compares every option across both
// fixtures; if the resolver produced different values for
// FANCY_FEATURE between the two forms, the round-trip test would
// notice.

use quon_config::{Config, ConfigValue, Expr, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("kconfig-minimal", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("default")
        .target(target)
        .opt_level(0)
        .debug_info(true);

    cfg.config_bool("LOG_ENABLED")
        .default_value(ConfigValue::Bool(true))
        .help("Enable runtime logging");

    cfg.config_u32("LOG_LEVEL")
        .default_value(ConfigValue::U32(3))
        .range(0, 5)
        .depends_on(Expr::Ident("LOG_ENABLED".into()));

    cfg.config_bool("DEBUG").default_value(ConfigValue::Bool(false));

    cfg.config_bool("FANCY_FEATURE")
        .default_value(ConfigValue::Bool(true))
        .depends_on(Expr::And(vec![
            Expr::Ident("LOG_ENABLED".into()),
            Expr::Not(Box::new(Expr::Ident("DEBUG".into()))),
        ]));

    cfg.preset("verbose")
        .set("LOG_LEVEL", ConfigValue::U64(5))
        .set("DEBUG", ConfigValue::Bool(true));

    Ok(())
}
