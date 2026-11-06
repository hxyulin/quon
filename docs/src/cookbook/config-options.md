# Kernel with config options

Quon includes a Kconfig-style configuration system that generates a Rust crate
of typed constants. This lets you define build-time options -- feature toggles,
numeric tuning knobs, string paths -- and access them as normal Rust constants
with full type safety.

## quon.rs

```rust
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("configurable_kernel", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    // Typed configuration options
    let log = q.add(
        opt::bool("LOG_ENABLED")
            .default(true)
            .help("Enable runtime logging subsystem"),
    );
    let level = q.add(
        opt::u32("LOG_LEVEL")
            .default(3)
            .range(0..=5)
            .depends_on(&log)
            .help("Verbosity: 0 = off, 5 = trace"),
    );
    let debug = q.add(opt::bool("DEBUG").help("Enable kernel debug assertions"));

    // Presets bundle option values
    q.add(Preset::new("verbose").set(&level, 5).set(&debug, true));
    q.add(Preset::new("quiet").set(&log, false));

    // A `Crate` links the generated config crate.
    q.add(Crate::bin("kernel", "crates/kernel"));

    Ok(())
}
```

## Using config values in Rust

The generated config crate is named `<project>_config`. Import it like any
other crate:

```rust
// In crates/kernel/src/main.rs
use configurable_kernel_config::*;

fn init_logging() {
    if LOG_ENABLED {
        set_log_level(LOG_LEVEL);
    }
}
```

Because these are `const` values, the compiler eliminates dead branches. When
`LOG_ENABLED` is `false`, the body of the `if` block is removed entirely -- no
runtime cost.

## Overriding values

There are several ways to override the defaults declared in `quon.rs`.

### Per-developer file (`.quon-config`)

Create a `.quon-config` file in your project root (typically gitignored):

```
LOG_LEVEL = 4
DEBUG = true
```

### Environment variables

Prefix the option name with `QUON_`:

```sh
QUON_LOG_LEVEL=5 quon build
```

Environment variables take precedence over `.quon-config`.

### Via preset (in a profile)

Presets bundle multiple overrides into a named set. Apply one in a
profile, with the `PresetRef` that `q.add(Preset::new(..))` returned:

```rust,ignore
let verbose = q.add(Preset::new("verbose").set(&level, 5).set(&debug, true));
let dev = q.profile("dev");
q.add(Profile::new("verbose-dev").inherits(&dev).preset(&verbose));
```

`quon build -p verbose-dev` then builds with the preset's values.

### Precedence order

From lowest to highest priority:

1. `.default(value)` in `quon.rs`
2. Preset applied by the active profile
3. `.quon-config` file
4. `--config-file` flag (overrides the `.quon-config` path)
5. `QUON_*` environment variables

## Using Kconfig files instead

If you prefer the traditional Kconfig format, replace inline declarations with
a `.kconfig` file:

```rust,ignore
q.include_kconfig("options.kconfig");
```

```kconfig
# options.kconfig
config LOG_ENABLED
    bool "Enable logging"
    default y

config LOG_LEVEL
    int "Log verbosity (0-5)"
    default 3
    range 0 5
    depends on LOG_ENABLED
```

Both approaches produce the same generated config crate. Use whichever fits
your project better -- inline declarations for small projects, Kconfig files
for larger ones with many options.

`examples/with-config/` in the Quon repository is a runnable version of
this project, with the kernel declared in a Cargo workspace. A
`Package` links the config crate only with `.link_config()`.
