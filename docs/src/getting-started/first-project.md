# Your First Project

This page walks through creating a minimal bare-metal kernel project with
Gluon. By the end you will have a `gluon.rs` configuration file and a
directory layout ready to build.

## Directory layout

```
my-kernel/
  gluon.rs
  crates/kernel/
    src/main.rs
    kernel.ld
```

`gluon.rs` is the single configuration file that declares your entire build.
It lives at the root of your project.

## Writing gluon.rs

A minimal configuration needs four declarations: a project, a target, a
profile, and a group containing at least one crate.

```rust
use gluon_config::{Config, CrateType, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("my-kernel", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .run_binary("kernel");

    cfg.group("kernel")
        .target(target)
        .edition("2021")
        .add("kernel", "crates/kernel")
            .crate_type(CrateType::Bin)
            .root("src/main.rs")
            .linker_script("crates/kernel/kernel.ld");

    Ok(())
}
```

### What each declaration does

- **`project(name, version)`** -- declares the project name and version. This
  is metadata used in build output paths and diagnostics.

- **`target(triple)`** -- registers a rustc target triple. Gluon will compile a
  custom sysroot for each registered target. You can register multiple targets
  if your project needs them (e.g., one for the kernel, another for a
  bootloader).

- **`profile(name)`** -- defines a compilation profile (analogous to Cargo's
  `dev` / `release` profiles). `.target()` sets which target triple this
  profile compiles for, `.opt_level()` and `.debug_info()` control optimization
  and debug symbols, and `.run_binary()` names the crate whose output is the
  final bootable artifact.

- **`group(name)`** -- collects one or more crates that share a target and
  edition. `.add(name, path)` registers a crate within the group, returning a
  crate builder where you set the crate type, source root, linker script, and
  other per-crate options.

## Adding QEMU configuration

If you want to launch your kernel in QEMU with `gluon run`, declare a QEMU
runner and name it in the profile:

```rust,ignore
cfg.runner("qemu")
    .qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(128)
    .cores(1)
    .serial_stdio();

cfg.profile("dev")
    .target(target)
    .run_binary("kernel")
    .runner("qemu");
```

- **`runner(name)`** -- declares a [runner](../running/runners.md), the
  thing that starts the built kernel. A profile picks one with
  `.runner(name)`.
- **`.qemu(binary)`** -- the QEMU system emulator binary to invoke.
- **`.machine(type)`** -- the machine/board model (`q35`, `virt`, etc.).
- **`.memory(mb)`** -- guest RAM in megabytes.
- **`.cores(n)`** -- number of virtual CPU cores.
- **`.serial_stdio()`** -- redirects the guest serial port to the host terminal,
  so `print!` / serial output appears in your shell.

## Next steps

With your `gluon.rs` and source files in place, head to
[Building and Running](./building-and-running.md) to compile and launch your
kernel.
