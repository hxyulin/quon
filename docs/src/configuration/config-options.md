# Config options and Kconfig

Quon supports typed configuration options that become compile-time constants
in an auto-generated Rust crate. This is similar in spirit to Linux's Kconfig
system, adapted for Rust's type system.

## Inline config options

Declare an option with the function for its type, in the `opt`
module, and add it. `q.add` returns an `OptRef` handle typed by the
option's kind:

```rust,ignore
let log = q.add(opt::bool("LOG_ENABLED").default(true).help("Enable runtime logging subsystem"));

let level = q.add(
    opt::u32("LOG_LEVEL")
        .default(3)
        .range(0..=5)
        .depends_on(&log)
        .help("Verbosity: 0 = off, 5 = trace"),
);

q.add(opt::str("KERNEL_NAME").default("my-kernel").help("Kernel display name"));
```

### Available types

| Function | Value | Default |
|---|---|---|
| `opt::bool(name)` | `bool` | `false` |
| `opt::tristate(name)` | `TristateVal`: yes, module or no | no |
| `opt::u32(name)` | `u32` | 0 |
| `opt::u64(name)` | `u64` | 0 |
| `opt::str(name)` | a string | empty |
| `opt::choice(name, [..])` | one of the listed names | the first |
| `opt::list(name)` | a list of strings, with no constant in the config crate | empty |

`.default(value)` takes a value of the option's type, so
`opt::u32("X").default("3")` does not compile.

### Common methods

| Method | Description |
|---|---|
| `.default(value)` | The value when no preset or override sets one |
| `.help(text)` | Human-readable description |
| `.menu(name)` | The menu the option belongs to, for tools that list options |
| `.range(lo..=hi)` | Valid range, for `u32` and `u64` options |
| `.depends_on(cond)` | Turn the option off unless `cond` holds |
| `.selects(&other)` | Turn the `bool` or `tristate` option `other` on whenever this one is on. Call it once per option |
| `.binding(binding)` | Also expose the option through `binding`, such as `Binding::Cfg` |

### Conditions

`.depends_on` takes a condition built from the handles of `bool` and
`tristate` options, combined with `&`, `|` and `!`:

```rust,ignore
let log = q.add(opt::bool("LOG"));
let debug = q.add(opt::bool("DEBUG"));
q.add(opt::bool("FANCY").depends_on(&log & !&debug));
```

`Cond::named("X")` refers to an option declared in a `.kconfig` file,
which has no handle: `.depends_on(&log & Cond::named("EXTRA"))`.

## Presets

A preset is a named set of option values:

```rust,ignore
let verbose = q.add(Preset::new("verbose").set(&level, 5).set(&debug, true));
```

`.set(&option, value)` checks the value's type against the option's.
`.set_named("X", ConfigValue::U32(5))` sets an option declared in a
`.kconfig` file, and Quon checks the type when it resolves the
preset. `.inherits(&other)` starts from another preset's values.

A profile applies a preset:

```rust,ignore
let dev = q.profile("dev");
q.add(Profile::new("verbose-dev").inherits(&dev).preset(&verbose));
```

## The generated config crate

Config options are compiled into an auto-generated Rust crate named
`<project>_config`, after the name given to `q.project`. It exports
each option, except list options, as a public constant:

```rust,ignore
use my_kernel_config::*;

if LOG_ENABLED {
    set_log_level(LOG_LEVEL);
}
```

A crate declared with `Crate` links the config crate whenever it links
the sysroot. A `Package` links it only with `.link_config()`, so its
workspace dependencies don't.

## Kconfig files

For projects with many configuration options, you can load them from a
Linux-style Kconfig file instead of (or in addition to) declaring them inline:

```rust,ignore
q.include_kconfig("options.kconfig");
```

The path is relative to the project root. Quon reads the file after
the script runs.

### Supported Kconfig syntax

Quon's Kconfig parser supports the standard constructs:

- **Entries**: `config`, `menuconfig`
- **Grouping**: `menu` / `endmenu`
- **Types**: `bool`, `int`, `string`, `tristate`
- **Properties**: `default`, `depends on`, `select`, `help`
- **Includes**: `source` for pulling in other Kconfig files
- **Expressions**: full `&&`, `||`, `!` grammar in conditions

Kconfig options and options declared with `opt::*` share one
namespace. You can declare some options inline and load others from
Kconfig files, as long as no name repeats.
