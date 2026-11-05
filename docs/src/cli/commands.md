# Commands and Flags

Complete CLI reference for Gluon.

## Global Flags

These flags apply to all commands.

| Flag | Short | Description |
|------|-------|-------------|
| `--profile <NAME>` | `-p` | Profile to use (overrides default) |
| `--target <NAME>` | `-t` | Target triple (overrides profile's target) |
| `--config-file <PATH>` | `-C` | Override file path (default: `.gluon-config`) |
| `--jobs <N>` | `-j` | Parallel compile jobs (default: host parallelism) |
| `--verbose` | `-v` | Print `Fresh` lines for cache hits, why each job reruns, and the rustc command of a failed job |
| `--quiet` | `-q` | Print only warnings and errors |
| `--color <WHEN>` | | `auto` (default), `always` or `never`. `auto` colors when stderr is a terminal and `NO_COLOR` is unset |

## Commands

### `gluon init`

Write a `gluon.rs` for the Cargo workspace in the current directory. Gluon
reads the target, kernel package, linker script and version from the
workspace and its toolchain files, and writes a `// TODO:` comment for each
setting it cannot find. See [Generating gluon.rs](../getting-started/init.md).

| Flag | Description |
|------|-------------|
| `--target <TRIPLE>` | Target triple or `.json` spec; repeatable. Overrides inference. |
| `--kernel <PACKAGE>` | Workspace member that holds the kernel. |
| `--bootloader limine\|uefi\|none` | How `gluon run` boots the kernel. Default: `none`. |
| `--limine <VERSION>` | Pin a Limine release with an empty hash; the first build prints the hash. |
| `--force` | Overwrite an existing `gluon.rs`. |
| `--dry-run` | Print the file instead of writing it. |

```sh
gluon init
gluon init --bootloader limine --limine 11.4.1
```

### `gluon build`

Build the project. Compiles all crates in the build model using the resolved
profile. This includes compiling a custom sysroot (`core`, `alloc`,
`compiler_builtins`) if one does not already exist for the target, then
compiling each crate with per-crate flag assembly.

```sh
gluon build
gluon build -p release
gluon build -t aarch64-unknown-none
```

### `gluon check`

Run a metadata-only check pass over every crate. Equivalent to `cargo check`
but uses Gluon's per-crate flag assembly. This is faster than a full build
because it skips code generation. Output goes to `build/tool/check/`.

```sh
gluon check
```

### `gluon clippy`

Run clippy over every crate. Uses `clippy-driver`, resolved in the following
order:

1. The `$CLIPPY_DRIVER` environment variable
2. A `clippy-driver` binary next to the active `rustc`
3. `clippy-driver` on `$PATH`

Output goes to `build/tool/clippy/`.

```sh
gluon clippy
```

### `gluon fmt [--check]`

Run `rustfmt` over every crate in the build model.

| Flag | Description |
|------|-------------|
| `--check` | Verify formatting without rewriting files. Exits non-zero if any file is unformatted. |

```sh
gluon fmt
gluon fmt --check
```

### `gluon clean [--keep-sysroot]`

Remove the `build/` directory.

| Flag | Description |
|------|-------------|
| `--keep-sysroot` | Preserve the sysroot directory, avoiding expensive recompilation of `core`, `alloc`, and `compiler_builtins` on the next build. |

```sh
gluon clean
gluon clean --keep-sysroot
```

### `gluon configure [--output PATH]`

Generate a `rust-project.json` file for rust-analyzer. This gives your editor
a complete project model even though Gluon does not use `Cargo.toml`.

| Flag | Short | Description |
|------|-------|-------------|
| `--output` | `-o` | Output path (default: `<project_root>/rust-project.json`) |

```sh
gluon configure
gluon configure -o ./ide/rust-project.json
```

See [rust-analyzer](../tooling/rust-analyzer.md) for details on editor
integration.

### `gluon vendor [--check] [--force] [--offline]`

Vendor external dependencies into the project.

| Flag | Description |
|------|-------------|
| `--check` | Verify vendor tree integrity without modifying anything. Exits non-zero if the vendor directory has drifted from the lockfile. |
| `--force` | Re-run `cargo vendor` unconditionally, even if the vendor directory appears fresh. |
| `--offline` | No network access. The lockfile must already exist. |

```sh
gluon vendor
gluon vendor --check
gluon vendor --force
```

### `gluon run [flags] [-- QEMU_ARGS...]`

Build the project and launch it in QEMU. Arguments after `--` are passed
directly to QEMU.

| Flag | Short | Description |
|------|-------|-------------|
| `--uefi` | | Force UEFI boot (conflicts with `--direct`) |
| `--direct` | | Force direct kernel boot (conflicts with `--uefi`) |
| `--timeout <SECS>` | `-T` | Kill QEMU after N seconds |
| `--dry-run` | | Print the QEMU command without executing it |
| `--no-build` | | Skip the build step (use existing artifacts) |
| `--gdb` | | Start with a GDB server, halting at the first instruction |

```sh
gluon run
gluon run -p release
gluon run --uefi --timeout 30
gluon run --gdb -- -d int
gluon run --dry-run
```

### `gluon test [flags] [FILTER] [-- QEMU_ARGS...]`

Build the integration tests and boot each one under QEMU, one at a time,
in name order. A test passes when QEMU exits with
`qemu().success_exit_code(...)`, or 0 when that is unset. Only the tests
whose name contains `FILTER` run. See
[Integration Tests](../running/integration-tests.md).

| Flag | Short | Description |
|------|-------|-------------|
| `--uefi` | | Force UEFI boot (conflicts with `--direct`) |
| `--direct` | | Force direct kernel boot |
| `--timeout <SECS>` | `-T` | Per-test timeout (default: the profile's `test_timeout`, then `qemu().test_timeout(...)`, then 60) |
| `--dry-run` | | Print each test's QEMU command without building or booting |
| `--no-build` | | Skip the build step (use existing artifacts) |

```sh
gluon test
gluon test heap
gluon test --timeout 120 -- -smp 4
```

## External Subcommands

Unknown subcommands are dispatched to `gluon-<name>` binaries on `$PATH`. For
example:

```sh
gluon foo bar
```

This runs `gluon-foo bar`, passing all remaining arguments. This mechanism
enables plugin development without modifying Gluon itself.
