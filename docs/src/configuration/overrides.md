# Configuration overrides

Config option values are resolved through a three-layer precedence system.
From highest to lowest priority:

1. **`QUON_*` environment variables** (highest priority)
2. **`.quon-config` file** (per-developer overrides)
3. **`quon.rs` defaults** (lowest priority)

This design lets the `quon.rs` file declare sensible defaults, individual
developers can override them locally without modifying the checked-in config,
and CI or one-off builds can override everything via environment variables.

## The .quon-config file

A simple `KEY = value` file placed in the project root:

```
# Per-developer overrides
LOG_ENABLED = true
LOG_LEVEL = 5
KERNEL_NAME = "debug-kernel"
```

### Syntax rules

- Lines starting with `#` are comments.
- Boolean values: `true`, `false`.
- Integer values: decimal numbers.
- String values: quoted with double quotes.
- Whitespace around `=` is optional.

The `.quon-config` file is typically added to `.gitignore` so each developer
can maintain their own overrides without creating merge conflicts.

The command line can name another file with `-C` / `--config-file`.

`RUN` is reserved. It names the
[run entry](../running/runners.md#several-run-entries) a command uses
when the command line names none:

```
RUN = "hero-gimbal"
```

## Environment variables

Prefix any config option name with `QUON_` to override it via the environment:

```sh
QUON_LOG_LEVEL=5 quon build
QUON_DEBUG=true quon build
```

Environment variables always take the highest precedence. This is particularly
useful in CI pipelines where you want to force specific settings without
modifying any files.

## Where presets fit in

Presets, applied with `Profile::preset(&preset)`, sit between the
`quon.rs` defaults and the `.quon-config` overrides. The full
precedence order is:

1. `QUON_*` environment variables
2. `.quon-config` file
3. The active profile's preset
4. The `.default(value)` of each option in `quon.rs`

## Precedence example

Given these three layers:

```rust,ignore
// quon.rs
q.add(opt::u32("LOG_LEVEL").default(3));
```

```
# .quon-config
LOG_LEVEL = 4
```

```sh
# Environment
QUON_LOG_LEVEL=5 quon build
```

The resolved value of `LOG_LEVEL` is **5** -- the environment variable wins.

Remove the environment variable, and the value becomes **4** (from
`.quon-config`). Remove the `.quon-config` entry too, and it falls back to
**3** (the default declared in `quon.rs`).
