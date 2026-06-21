# Gluon

> [!WARNING]
> **This project is archived.** Gluon was a from-scratch build system that
> drove `rustc` directly and built its own sysroots — effectively a
> hand-rolled `-Z build-std`. Since it began, the bare-metal Rust
> ecosystem matured to the point where Cargo covers the same ground with
> far less to maintain:
>
> - **Sysroots** → `-Z build-std`
> - **Bootloader/kernel two-triple handoff** → `-Z bindeps` (artifact
>   dependencies)
> - **Per-crate linker scripts / cfgs** → a crate's own `build.rs`
> - **Running in QEMU** → the `runner` key in `.cargo/config.toml`
>
> Maintaining a full build system to reimplement those (and to bridge back
> to Cargo for `build.rs`/proc-macro deps) stopped being worth it. The
> recommended path is now **Cargo + a `cargo xtask`**.
>
> The parts of Gluon that pulled their weight independently of the build
> graph — disk-image assembly, QEMU launching, Kconfig — have been
> extracted into a reusable library:
> **[embedded-build-tools](https://github.com/hxyulin/embedded-build-tools)**.
>
> This repository remains available, read-only, as a reference and a
> record of the design.

Gluon is a build system for bare-metal Rust kernels. It bypasses Cargo,
drives `rustc` directly, builds custom sysroots, and orchestrates the full
pipeline from configuration through compilation to bootable artifacts and
QEMU runs.

## Why Gluon?

Cargo is excellent for most Rust projects, but bare-metal kernel development
pushes against its assumptions:

- **Custom sysroots.** Bare-metal targets need `core`, `alloc`, and
  `compiler_builtins` compiled from source with specific flags. Cargo's
  `-Zbuild-std` is unstable and gives limited control over sysroot
  compilation flags.
- **Multiple targets in one project.** A UEFI bootloader and a bare-metal
  kernel live in the same repo but compile for different triples. Cargo
  workspaces do not natively support per-crate target overrides.
- **Linker scripts and post-build steps.** Kernels need custom linker
  scripts, binary transformations, and ESP assembly. Cargo has no built-in
  mechanism for these; projects resort to fragile `build.rs` scripts and
  shell wrappers.
- **QEMU integration.** The edit-compile-boot loop is the inner loop of
  kernel development. Gluon makes `gluon run` a single command that
  builds, assembles boot artifacts, and launches QEMU.
- **Compile-time configuration.** Kernels need Kconfig-style option systems
  for conditional compilation across hundreds of features. Cargo features
  are flat and boolean; Gluon provides typed options with dependencies,
  ranges, presets, and override layers.

Gluon replaces this patchwork with a single tool driven by a declarative
Rust build script.

## Quick Start

**Requirements:**

- Rust nightly toolchain (1.85+) with the `rust-src` component
- QEMU (for `gluon run`; not needed for just building)

**Install:**

```sh
git clone https://github.com/hxyulin/gluon.git
cd gluon
cargo install --path crates/gluon-cli
```

**Create `gluon.rs` in your project root:**

```rust
use gluon_config::{Config, CrateType, Result};

fn main() -> Result<()> {
    let mut cfg = Config::new();

    cfg.project("my-kernel", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .boot_binary("kernel");

    cfg.group("kernel")
        .target(target)
        .edition("2021")
        .add("kernel", "crates/kernel")
            .crate_type(CrateType::Bin)
            .root("src/main.rs")
            .linker_script("crates/kernel/kernel.ld");

    cfg.qemu("qemu-system-x86_64")
        .machine("q35")
        .memory(128)
        .cores(1)
        .serial_stdio();

    cfg.emit()
}
```

See [the gluon.rs guide](docs/src/configuration/gluon-rs.md) for details on
the build script API, including how to
[split config across multiple files](docs/src/configuration/gluon-rs.md#splitting-config-across-files)
in larger projects.

**Build and run:**

```sh
gluon build              # Compile everything
gluon run                # Build + launch QEMU
gluon check              # Metadata-only check pass
gluon configure          # Generate rust-project.json for rust-analyzer
```

## Features

- **Direct rustc invocation** -- per-crate flag assembly, no Cargo overhead
- **Custom sysroot compilation** -- `core`, `alloc`, `compiler_builtins` built for your target
- **Rust build script** -- type-safe `gluon.rs` with projects, targets, profiles, groups, and crates
- **Kconfig-style options** -- typed config options with defaults, ranges, dependencies, and presets
- **Configuration overrides** -- `gluon.rs` defaults, `.gluon-config` file, and `GLUON_*` environment variables
- **Hybrid build cache** -- two-tier mtime + SHA-256 freshness checking
- **Parallel DAG scheduler** -- dependency-aware build graph with worker pool
- **Dependency vendoring** -- `gluon vendor` with lockfile and fingerprinting
- **QEMU orchestration** -- direct boot, UEFI boot (OVMF), GDB server, timeouts
- **rust-analyzer integration** -- `gluon configure` generates `rust-project.json`
- **Pipeline and rule system** -- user-defined build stages with copy, mkimage, and exec rules
- **ESP assembly** -- automatic EFI System Partition layout for UEFI boot
- **External plugins** -- `gluon foo` dispatches to `gluon-foo` on `$PATH`

## Documentation

Full documentation is available in [`docs/`](docs/) (built with [mdbook](https://rust-lang.github.io/mdBook/)).

```sh
cd docs && mdbook serve
```

## License

Gluon is licensed under the [GNU General Public License v3.0](LICENSE).
