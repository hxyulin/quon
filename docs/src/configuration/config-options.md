# Config Options and Kconfig

Gluon supports typed configuration options that become compile-time constants
in an auto-generated Rust crate. This is similar in spirit to Linux's Kconfig
system, adapted for Rust's type system.

## Inline config options

Declare config options directly in `gluon.rs` using the typed builder
functions:

```rust
use gluon_config::{ConfigValue, Expr};

cfg.config_bool("LOG_ENABLED")
    .default_value(ConfigValue::Bool(true))
    .help("Enable runtime logging subsystem");

cfg.config_u32("LOG_LEVEL")
    .default_value(ConfigValue::U32(3))
    .range(0, 5)
    .depends_on(Expr::Ident("LOG_ENABLED".into()))
    .help("Verbosity: 0 = off, 5 = trace");

cfg.config_str("KERNEL_NAME")
    .default_value(ConfigValue::Str("my-kernel".into()))
    .help("Kernel display name");
```

### Available types

| Function | Rust type | Description |
|---|---|---|
| `config_bool(name)` | `bool` | Boolean option |
| `config_u32(name)` | `u32` | Unsigned 32-bit integer |
| `config_str(name)` | `&str` | String constant |

### Common methods

| Method | Description |
|---|---|
| `.default_value(val)` | Default when no override is applied (pass `ConfigValue::Bool(true)`, etc.) |
| `.help("...")` | Human-readable description |
| `.range(lo, hi)` | Valid range (integers only) |
| `.depends_on(expr)` | Only active when `expr` evaluates to true (pass an `Expr` value) |

When a config option has a `.depends_on()` constraint and the dependency
evaluates to false, the option is disabled and uses its default value (or is
omitted entirely, depending on the type).

## Presets

Presets are named bundles of config overrides:

```rust
use gluon_config::ConfigValue;

cfg.preset("verbose")
    .set("LOG_LEVEL", ConfigValue::U32(5))
    .set("DEBUG", ConfigValue::Bool(true));
```

Apply a preset to a profile:

```rust
let target = cfg.target("x86_64-unknown-none");

cfg.profile("dev")
    .target(target)
    .preset("verbose");
```

Presets are useful for defining common configurations that multiple profiles
can share (e.g., a "verbose" preset for development, a "minimal" preset for
release).

## The generated config crate

Config options are compiled into an auto-generated Rust crate named
`<project>_config` by default. You can customize this name with
`project().config_crate_name()` (see
[Projects, Targets, and Profiles](./projects-targets-profiles.md)).

The generated crate exports each config option as a public constant:

```rust
use my_kernel_config::*;

if LOG_ENABLED {
    set_log_level(LOG_LEVEL);
}
```

Groups with `.config(true)` automatically link against the config crate. For
individual crates, use `.requires_config(true)` to opt in or
`.requires_config(false)` to opt out regardless of the group setting.

## Kconfig files

For projects with many configuration options, you can load them from a
Linux-style Kconfig file instead of (or in addition to) declaring them inline:

```rust
cfg.include_kconfig("./options.kconfig")?;
```

The path is relative to the `gluon.rs` file. The `?` operator propagates errors
out of `config()`, which returns `Result<()>`.

### Supported Kconfig syntax

Gluon's Kconfig parser supports the standard constructs:

- **Entries**: `config`, `menuconfig`
- **Grouping**: `menu` / `endmenu`
- **Types**: `bool`, `int`, `string`, `tristate`
- **Properties**: `default`, `depends on`, `select`, `help`
- **Includes**: `source` for pulling in other Kconfig files
- **Expressions**: full `&&`, `||`, `!` grammar in conditions

Kconfig options and inline `config_*()` declarations coexist in the same
namespace. You can declare some options inline and load others from Kconfig
files -- just avoid duplicate names.
