# FAQ

## Why not just use Cargo?

Quon works with Cargo rather than around it: it reads the Cargo
manifests on every build. It adds what Cargo leaves to scripts:

- **Several targets in one build**, each group of crates for its own
  triple.
- **Custom sysroot compilation** without the unstable `-Zbuild-std`
  flag, with control over the flags.
- **Per-crate compiler flags** and linker scripts.
- **Post-build steps and runners**: image assembly, ESP layout, and
  `quon run`/`quon test` on the host, through a command or in QEMU.

For a project that needs none of this, Cargo is enough.

## What Rust toolchain do I need?

Rust 1.99 or later. Stable works for host builds and for built-in
targets such as `x86_64-unknown-none`. A custom target-spec `.json`
needs nightly, because rustc accepts one only with `-Zunstable-options`.

Targets that Quon builds a sysroot for need the `rust-src` component.
If your project includes a `rust-toolchain.toml`, rustup installs and
uses the toolchain it names.

```sh
rustup component add rust-src
```

## How do I add a new target?

Add a `target()` declaration in `quon.rs`:

```rust
let target = cfg.target("aarch64-unknown-none");
```

For custom target specs, provide a path to the JSON file:

```rust
let target = cfg.target_spec("my-custom-target", "./targets/my-custom-target.json");
```

Quon will compile a separate sysroot for each declared target.

## Where are build artifacts?

In the `build/` directory under your project root:

| Path | Contents |
|------|----------|
| `build/cross/<target>/<profile>/deps/` | Compiled crate outputs |
| `build/sysroot/<target>/` | Custom sysroot (`core`, `alloc`, `compiler_builtins`) |
| `build/tool/check/` | `quon check` output |
| `build/tool/clippy/` | `quon clippy` output |

## How do I use Quon with an existing project?

Run `quon init` in the directory with the root `Cargo.toml`. It
writes a `quon.rs` from the manifests and toolchain files: a host
build when no target is set, a kernel build otherwise. See
[Generating quon.rs](./getting-started/init.md). Quon needs no
changes to your Rust source code.

## Can I use Quon for projects that are not kernels?

Yes. A profile with `.target_host()` builds for the build machine, and
`quon run` runs the binary directly. Quon's own repository builds
this way. For bare-metal projects other than kernels (firmware,
bootloaders, embedded images), declare their targets as for a kernel.

## How do I integrate with CI?

Quon is a single binary. Install it in CI the same way as locally:

```sh
cargo install --path crates/quon
quon build -j 1    # Deterministic single-threaded for reproducibility
```

Useful CI checks:

- `quon fmt --check` -- enforce formatting
- `quon clippy` -- lint all crates
- `quon vendor --check` -- verify vendored dependencies have not drifted
- `quon check` -- fast metadata-only build verification

## How do I vendor dependencies?

```sh
quon vendor           # Populate ./vendor/ and write quon.lock
```

Commit both `vendor/` and `quon.lock` to version control. Subsequent builds
auto-detect vendored crates. Use `quon vendor --check` in CI to ensure the
vendor tree stays in sync. See [Dependencies](./configuration/dependencies.md)
for details.

## How do I debug my kernel in QEMU?

Use the `--gdb` flag:

```sh
quon run --gdb
```

This starts QEMU with a GDB server, halted at the first instruction. Connect
from another terminal:

```sh
gdb -ex "target remote :1234" build/cross/<target>/<profile>/deps/kernel
```

## How do I pass extra flags to QEMU?

Arguments after `--` are forwarded to QEMU verbatim:

```sh
quon run -- -d int -no-reboot
```

## How do I see what rustc commands Quon runs?

Use the `--verbose` flag:

```sh
quon build --verbose
```

This prints each `rustc` invocation with its full argument list.
