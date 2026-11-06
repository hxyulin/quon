# UEFI bootloader and kernel

This is the most advanced cookbook example: a UEFI bootloader that embeds a
bare-metal kernel binary, with ESP assembly and QEMU integration. It
demonstrates multi-target builds, cross-crate artifact dependencies, and UEFI
boot configuration.

## quon.rs

```rust
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("my-os", "0.1.0");

    // Two targets: bare-metal for the kernel, UEFI for the bootloader
    let kernel_target = Target::builtin("x86_64-unknown-none");
    let uefi_target = Target::builtin("x86_64-unknown-uefi");

    let kernel = q.add(
        Crate::bin("kernel", "crates/kernel")
            .target(&kernel_target)
            .linker_script("crates/kernel/kernel.ld"),
    );

    // artifact_env passes the kernel's output path and builds it first
    let bootloader = q.add(
        Crate::bin("bootloader", "crates/bootloader")
            .target(&uefi_target)
            .artifact_env("KERNEL_PATH", &kernel),
    );

    // Assemble an EFI System Partition
    q.add(Esp::new("default").add(&bootloader, "EFI/BOOT/BOOTX64.EFI"));

    // QEMU with UEFI boot (requires OVMF)
    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(256)
            .cores(1)
            .serial(Serial::Stdio)
            .boot(BootMode::Uefi),
    );

    // Profiles build for the bootloader's target, the UEFI one.
    q.run(&bootloader).runner(&qemu);
    q.add(Profile::release().lto(Lto::Thin));

    Ok(())
}
```

## Directory layout

```
my-os/
  quon.rs
  crates/
    kernel/
      src/main.rs
      kernel.ld
    bootloader/
      src/main.rs
```

## How it works

The build has four stages:

### 1. Kernel compiles first

The `artifact_env("KERNEL_PATH", &kernel)` declaration on the bootloader crate
does two things: it creates an automatic build dependency so the kernel is
always compiled before the bootloader, and it sets the `KERNEL_PATH` environment
variable to the absolute path of the kernel binary during bootloader
compilation.

### 2. Bootloader embeds the kernel

At compile time, the bootloader uses `env!()` and `include_bytes!()` to embed
the kernel binary directly:

```rust
static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));
```

This means the bootloader EFI binary is self-contained -- it carries the kernel
payload inside itself.

### 3. ESP assembly

The `Esp` tells Quon to copy the compiled bootloader binary into an
EFI System Partition layout. `.add(&bootloader, "EFI/BOOT/BOOTX64.EFI")`
places it at the standard UEFI boot path. Quon assembles the ESP as a
directory, and a UEFI boot mounts it when it is the project's only ESP.

### 4. QEMU launches with UEFI

The `.boot(BootMode::Uefi)` setting tells Quon to pass OVMF firmware flags to
QEMU. OVMF loads the bootloader from the ESP, which then loads the embedded
kernel into memory and transfers control to it.

## Running

```sh
quon run                 # Build + UEFI boot in QEMU
quon run -r              # Use the release profile
quon run --gdb           # Start with GDB server for debugging
quon run --dry-run       # Print QEMU command without executing
```

## OVMF setup

Quon attempts to auto-detect OVMF firmware files. If detection fails, set
the paths explicitly via environment variables:

```sh
export OVMF_CODE=/usr/share/OVMF/OVMF_CODE.fd
export OVMF_VARS=/usr/share/OVMF/OVMF_VARS.fd
```

Common locations by distribution:

| Distribution | Typical path |
|-------------|-------------|
| Ubuntu/Debian | `/usr/share/OVMF/` |
| Fedora | `/usr/share/edk2/ovmf/` |
| Arch Linux | `/usr/share/edk2-ovmf/x64/` |
| macOS (Homebrew) | `$(brew --prefix)/share/qemu/` |

## Multi-target builds

This example uses two target triples in the same build. Quon compiles a
separate sysroot for each target and builds each crate for the target its
`.target(&t)` names. The dependency graph spans target boundaries
-- the bootloader (UEFI target) depends on the kernel (bare-metal target) via
`artifact_env`.

`examples/uefi-with-bootloader/` in the Quon repository is a runnable
version of this project. Its kernel and bootloader are Cargo workspace
packages that share a `boot-info` package, which Quon compiles once
for each target.
