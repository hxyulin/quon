# Minimal Kernel

A walkthrough of the simplest useful Gluon project: a single bare-metal binary
compiled for `x86_64-unknown-none` and launched in QEMU.

## gluon.rs

```rust
use gluon_config::{Config, CrateType, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    // Every project starts with a name and version.
    cfg.project("my-kernel", "0.1.0");

    // Declare the target triple for the kernel.
    let target = cfg.target("x86_64-unknown-none");

    // A profile controls optimization, debug info, and which target to use.
    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .run_binary("kernel");

    // A group collects crates sharing a target and edition.
    cfg.group("kernel")
        .target(target)
        .edition("2021")
        .add("kernel", "crates/kernel")
            .crate_type(CrateType::Bin)
            .root("src/main.rs")
            .linker_script("crates/kernel/kernel.ld");

    // Optional: configure QEMU for `gluon run`.
    cfg.qemu("qemu-system-x86_64")
        .machine("q35")
        .memory(128)
        .cores(1)
        .serial_stdio();

    Ok(())
}
```

## Directory layout

```
my-kernel/
  gluon.rs
  crates/kernel/
    src/main.rs
    kernel.ld
```

The `main.rs` file is a `#![no_std]` / `#![no_main]` binary. The linker script
(`kernel.ld`) controls the memory layout of the output binary -- entry point,
section placement, and so on.

## Building

```sh
gluon build          # Compile the kernel
gluon check          # Quick metadata-only check
gluon run            # Build + launch in QEMU
gluon configure      # Generate rust-project.json for your editor
```

## What happens during `gluon build`

1. **Sysroot compilation** -- Gluon compiles a custom sysroot (`core`, `alloc`,
   `compiler_builtins`) for `x86_64-unknown-none`. This only happens on the
   first build or when the sysroot is invalidated.
2. **Crate compilation** -- The `kernel` crate is compiled as a binary with
   the specified linker script, edition, and optimization level.
3. **Output** -- The resulting binary lands in
   `build/cross/x86_64-unknown-none/dev/deps/`.

## What happens during `gluon run`

After a successful build, Gluon launches QEMU with the configuration from the
`cfg.qemu()` block. The `run_binary("kernel")` setting in the profile tells Gluon
which crate's output to pass to QEMU as the kernel image. Serial output is
forwarded to your terminal via `.serial_stdio()`.

## Next steps

- Add config options: see [Kernel with Config Options](./config-options.md)
- Add a UEFI bootloader: see [UEFI Bootloader + Kernel](./uefi-bootloader.md)
- Set up your editor: see [Editor Setup](../tooling/editor-setup.md)

`examples/basic-kernel/` in the Gluon repository is a runnable version of
this project. It declares the kernel in a Cargo workspace and builds it
with `cfg.cargo_workspace(".")` and `group.package("kernel")`.
