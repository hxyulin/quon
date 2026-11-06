# Commands and Flags

Complete CLI reference for Gluon.

## Global Flags

These flags apply to all commands.

| Flag | Short | Description |
|------|-------|-------------|
| `--profile <NAME>` | `-p` | Profile to use (overrides default) |
| `--release` | `-r` | Use the `release` profile; same as `-p release` |
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

### `gluon setup`

Build the gluon-config library that `gluon.rs` links against, for the
rustc the current directory uses, and print the library's directory.
The library is built from the sources embedded in `gluon`, once per
Gluon version and rustc, into
`$GLUON_HOME` (default `~/.gluon`) and shared by every project. Other
commands build it on first use, so `setup` is only needed to do that
ahead of time, for example before going offline. See
[the gluon-config library](../configuration/gluon-rs.md#the-gluon-config-library-and-gluon).

```sh
gluon setup
```

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

### `gluon run [flags] [-- RUNNER_ARGS...]`

Build the project and start the profile's run binary with its
[runner](../running/runners.md). Arguments after `--` are appended to
the runner's: QEMU's, or a command runner's argv. `--gdb`, `--uefi`,
`--direct` and `--bios` need a QEMU runner.

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

Build the active profile with its tests, then run the host tests, the
integration tests and the boot tests, in that order. A failed run does
not stop the others; the command fails at the end if any run failed.
`FILTER` goes to each host test executable as libtest's filter, and
selects integration and boot tests by name. See
[Testing](../running/testing.md).

| Flag | Short | Description |
|------|-------|-------------|
| `--host` | | Run the host tests |
| `--integration` | | Run the integration tests |
| `--boot` | | Run the boot tests |
| `--uefi` | | Force UEFI boot (conflicts with `--direct`) |
| `--direct` | | Force direct kernel boot |
| `--timeout <SECS>` | `-T` | Per-boot timeout (default: the profile's `test_timeout`, then the QEMU runner's `.test_timeout(...)`, then 60) |
| `--dry-run` | | Print each test's command without building or running it |
| `--no-build` | | Skip the build step (use existing artifacts) |

With none of `--host`, `--integration` and `--boot`, every kind runs.
`QEMU_ARGS` apply to integration and boot tests.

```sh
gluon test
gluon test --host parse
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
