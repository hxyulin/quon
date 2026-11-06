# Your first project

This page walks through creating a minimal bare-metal kernel project with
Quon. By the end you will have a `quon.rs` configuration file and a
directory layout ready to build.

## Directory layout

```
my-kernel/
  quon.rs
  crates/kernel/
    src/main.rs
    kernel.ld
```

`quon.rs` is the single configuration file that declares your entire build.
It lives at the root of your project.

## Writing quon.rs

A minimal configuration names the project, the target, and at least
one crate:

```rust
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("my-kernel", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    q.add(Crate::bin("kernel", "crates/kernel").linker_script("crates/kernel/kernel.ld"));

    Ok(())
}
```

### What each declaration does

- **`q.project(name, version)`** declares the project name and version.
  Quon uses them in build output paths and diagnostics.

- **`q.default_target(&target)`** sets the target that crates build for
  when they name none. `Target::builtin(triple)` is a target rustc
  knows; install it with `rustup target add <triple>`. A
  crate can name its own with `.target(&t)`, for example a bootloader
  next to the kernel.

- **`q.add(Crate::bin(name, path))`** declares a binary crate with its
  sources in `path`, rooted at `src/main.rs`. `.linker_script(path)`
  links it with the given script. In a Cargo workspace, declare
  `Package::new(name)` instead, and Quon reads the rest from the
  package's `Cargo.toml`.

The project gets a `dev` and a `release` profile without declaring
them. They build for the kernel's target.

## Adding QEMU configuration

To launch your kernel in QEMU with `quon run`, declare a QEMU runner
and name the kernel as the run entry:

```rust,ignore
let kernel = q.add(Crate::bin("kernel", "crates/kernel").linker_script("crates/kernel/kernel.ld"));

let qemu = q.add(
    Qemu::new("qemu-system-x86_64")
        .machine("q35")
        .memory_mb(128)
        .cores(1)
        .serial(Serial::Stdio),
);
q.run(&kernel).runner(&qemu);
```

- **`Qemu::new(program)`** declares a [runner](../running/runners.md),
  the thing that starts the built kernel. `program` is the QEMU system
  emulator to invoke.
- **`.machine(type)`** is the machine model (`q35`, `virt`, etc.).
- **`.memory_mb(mb)`** is guest RAM in megabytes.
- **`.cores(n)`** is the number of virtual CPU cores.
- **`.serial(Serial::Stdio)`** connects the guest serial port to the
  host terminal, so serial output appears in your shell.
- **`q.run(&kernel).runner(&qemu)`** makes `quon run` boot the kernel
  with that runner.

## Next steps

With your `quon.rs` and source files in place, head to
[Building and Running](./building-and-running.md) to compile and launch your
kernel.
