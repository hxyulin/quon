# Minimal kernel

A walkthrough of the simplest useful Quon project: a single bare-metal binary
compiled for `x86_64-unknown-none` and launched in QEMU.

## quon.rs

```rust
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    // Every project starts with a name and version.
    q.project("my-kernel", "0.1.0");

    // Crates that name no target build for this one.
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    // A binary crate, rooted at crates/kernel/src/main.rs.
    let kernel = q.add(
        Crate::bin("kernel", "crates/kernel").linker_script("crates/kernel/kernel.ld"),
    );

    // QEMU, for `quon run`.
    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(128)
            .cores(1)
            .serial(Serial::Stdio),
    );

    // `quon run` boots the kernel under QEMU. The built-in `dev` and
    // `release` profiles build for the kernel's target.
    q.run(&kernel).runner(&qemu);

    Ok(())
}
```

## Directory layout

```
my-kernel/
  quon.rs
  crates/kernel/
    src/main.rs
    kernel.ld
```

The `main.rs` file is a `#![no_std]` / `#![no_main]` binary. The linker script
(`kernel.ld`) controls the memory layout of the output binary -- entry point,
section placement, and so on.

## Building

```sh
quon build          # Compile the kernel
quon check          # Quick metadata-only check
quon run            # Build + launch in QEMU
quon configure      # Generate rust-project.json for your editor
```

## What happens during `quon build`

1. **Sysroot compilation** -- Quon compiles a custom sysroot (`core`, `alloc`,
   `compiler_builtins`) for `x86_64-unknown-none`. This only happens on the
   first build or when the sysroot is invalidated.
2. **Crate compilation** -- The `kernel` crate is compiled as a binary with
   the linker script, the default edition (2024) and the `dev` profile's
   optimization level.
3. **Output** -- The resulting binary lands in
   `build/cross/x86_64-unknown-none/dev/deps/`.

## What happens during `quon run`

After a successful build, Quon launches QEMU with the settings of the
`Qemu` runner. `q.run(&kernel)` tells Quon which crate's output to pass
to QEMU as the kernel image, and `.runner(&qemu)` names the runner.
`.serial(Serial::Stdio)` forwards serial output to your terminal.

## Next steps

- Add config options: see [Kernel with Config Options](./config-options.md)
- Add a UEFI bootloader: see [UEFI Bootloader + Kernel](./uefi-bootloader.md)
- Set up your editor: see [Editor Setup](../tooling/editor-setup.md)

`examples/basic-kernel/` in the Quon repository is a runnable version of
this project. It declares the kernel in a Cargo workspace and builds it
with `q.cargo_workspace(".")` and `Package::new("kernel")`.
