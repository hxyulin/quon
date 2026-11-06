# Building and Running

This page covers the core CLI commands for compiling, running, and maintaining
your Quon project.

## Build

```sh
quon build
```

Compiles every crate in your project. Quon drives `rustc` directly for each
crate, assembling per-crate flags from your `quon.rs` configuration. Custom
sysroots (`core`, `alloc`, `compiler_builtins`) are compiled automatically for
each registered target triple.

Build output goes to the `build/` directory at your project root.

## Run

```sh
quon run
```

Builds the project (if needed) and starts the profile's run binary with the
profile's [runner](../running/runners.md), such as a QEMU runner declared
with `cfg.runner("qemu").qemu(...)`. The profile must name a runner with
`.runner(...)`.

To pass extra arguments directly to QEMU, place them after `--`:

```sh
quon run -- -serial mon:stdio -d int
```

## Check

```sh
quon check
```

Performs a metadata-only check pass, similar to `cargo check`. This runs `rustc`
with the check-only flag, catching type errors and borrow-checker issues without
producing final artifacts. Useful for fast feedback during development.

## Clippy

```sh
quon clippy
```

Runs `clippy-driver` on your crates, applying the same flag assembly and sysroot
that `quon build` uses. Requires the `clippy` component in your Rust toolchain.

## Format

```sh
quon fmt
```

Runs `rustfmt` on your project sources. To verify formatting without modifying
files (useful in CI), pass the check flag:

```sh
quon fmt --check
```

## Clean

```sh
quon clean
```

Removes the `build/` directory. To remove build artifacts but preserve the
compiled sysroot (which is expensive to rebuild), use:

```sh
quon clean --keep-sysroot
```

## Configure

```sh
quon configure
```

Generates a `rust-project.json` file for rust-analyzer. This gives your editor
full knowledge of the crate graph, sysroot paths, and compiler flags, enabling
accurate completions, diagnostics, and go-to-definition in bare-metal code.

## Common flags

These flags apply to most commands:

| Flag | Description |
|------|-------------|
| `-p <profile>` | Select a build profile (e.g., `-p release`). Defaults to the profile set by `.default_profile()` on `project()`, else the first profile in alphabetical order. |
| `-r` | Select the `release` profile. Shorthand for `-p release`. |
| `-t <triple>` | Override the target triple for this invocation. |
| `-j <N>` | Set the number of parallel jobs (e.g., `-j 4`). |

### Example: release build

```sh
quon build -r
```

### Example: build for a different target

```sh
quon build -t aarch64-unknown-none
```

## Setting a default profile

Rather than passing `-p` every time, you can set a default in your `quon.rs`:

```rust
cfg.project("my-kernel", "0.1.0")
    .default_profile("dev");
```

This is used whenever neither `-p` nor `-r` is given.
