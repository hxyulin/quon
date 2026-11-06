# FAQ

## Why not just use Cargo?

Gluon works with Cargo rather than around it: it reads the Cargo
manifests on every build. It adds what Cargo leaves to scripts:

- **Several targets in one build**, each group of crates for its own
  triple.
- **Custom sysroot compilation** without the unstable `-Zbuild-std`
  flag, with control over the flags.
- **Per-crate compiler flags** and linker scripts.
- **Post-build steps and runners**: image assembly, ESP layout, and
  `gluon run`/`gluon test` on the host, through a command or in QEMU.

For a project that needs none of this, Cargo is enough.

## What Rust toolchain do I need?

A nightly toolchain with the `rust-src` component. Gluon requires Rust 1.85 or
later. If your project includes a `rust-toolchain.toml`, rustup will
automatically install and use the correct version.

```sh
rustup component add rust-src
```

## How do I add a new target?

Add a `target()` declaration in `gluon.rs`:

```rust
let target = cfg.target("aarch64-unknown-none");
```

For custom target specs, provide a path to the JSON file:

```rust
let target = cfg.target_spec("my-custom-target", "./targets/my-custom-target.json");
```

Gluon will compile a separate sysroot for each declared target.

## Where are build artifacts?

In the `build/` directory under your project root:

| Path | Contents |
|------|----------|
| `build/cross/<target>/<profile>/deps/` | Compiled crate outputs |
| `build/sysroot/<target>/` | Custom sysroot (`core`, `alloc`, `compiler_builtins`) |
| `build/tool/check/` | `gluon check` output |
| `build/tool/clippy/` | `gluon clippy` output |

## How do I use Gluon with an existing project?

Run `gluon init` in the directory with the root `Cargo.toml`. It
writes a `gluon.rs` from the manifests and toolchain files: a host
build when no target is set, a kernel build otherwise. See
[Generating gluon.rs](./getting-started/init.md). Gluon needs no
changes to your Rust source code.

## Can I use Gluon for projects that are not kernels?

Yes. A profile with `.target_host()` builds for the build machine, and
`gluon run` runs the binary directly. Gluon's own repository builds
this way. For bare-metal projects other than kernels (firmware,
bootloaders, embedded images), declare their targets as for a kernel.

## How do I integrate with CI?

Gluon is a single binary. Install it in CI the same way as locally:

```sh
cargo install --path crates/gluon-cli
gluon build -j 1    # Deterministic single-threaded for reproducibility
```

Useful CI checks:

- `gluon fmt --check` -- enforce formatting
- `gluon clippy` -- lint all crates
- `gluon vendor --check` -- verify vendored dependencies have not drifted
- `gluon check` -- fast metadata-only build verification

## How do I vendor dependencies?

```sh
gluon vendor           # Populate ./vendor/ and write gluon.lock
```

Commit both `vendor/` and `gluon.lock` to version control. Subsequent builds
auto-detect vendored crates. Use `gluon vendor --check` in CI to ensure the
vendor tree stays in sync. See [Dependencies](./configuration/dependencies.md)
for details.

## How do I debug my kernel in QEMU?

Use the `--gdb` flag:

```sh
gluon run --gdb
```

This starts QEMU with a GDB server, halted at the first instruction. Connect
from another terminal:

```sh
gdb -ex "target remote :1234" build/cross/<target>/<profile>/deps/kernel
```

## How do I pass extra flags to QEMU?

Arguments after `--` are forwarded to QEMU verbatim:

```sh
gluon run -- -d int -no-reboot
```

## How do I see what rustc commands Gluon runs?

Use the `--verbose` flag:

```sh
gluon build --verbose
```

This prints each `rustc` invocation with its full argument list.
