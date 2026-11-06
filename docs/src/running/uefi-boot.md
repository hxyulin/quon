# UEFI boot and ESP assembly

UEFI boot is the standard boot path for modern x86_64 systems and is
required if your OS uses a UEFI bootloader. Quon handles the full
pipeline: compiling the bootloader for a UEFI target, assembling the
EFI System Partition, and configuring QEMU with OVMF firmware.

## Overview

UEFI boot requires three components:

1. **OVMF firmware** -- a UEFI implementation for QEMU that replaces the
   default BIOS.
2. **A UEFI application** -- your bootloader, compiled for a UEFI target
   triple like `x86_64-unknown-uefi`.
3. **An EFI System Partition (ESP)** -- a FAT-formatted partition
   containing the UEFI application at the standard path
   (`EFI/BOOT/BOOTX64.EFI` for x86_64).

## ESP assembly

Declare an ESP layout in `quon.rs`:

```rust,ignore
let bootloader = q.add(Package::new("bootloader").target(&uefi));
q.add(Esp::new("default").add(&bootloader, "EFI/BOOT/BOOTX64.EFI"));
```

`.add(source, esp_path)` copies `source` to the given path inside a
staging directory. Quon assembles the ESP after compilation. A source
is one of these:

- the handle of a crate, which copies its output binary;
- the handle of a `Download`;
- a string, which names a file in the project, relative to the project
  root. An edited file is copied again on the next build.

```rust,ignore
q.add(
    Esp::new("default")
        .add(&bootloader, "EFI/BOOT/BOOTX64.EFI")
        .add(&shell, "EFI/tools/Shell.efi")
        .add("limine.conf", "limine.conf"),
);
```

A destination must be relative to the ESP root. A UEFI QEMU boot
mounts the ESP when it is the project's only one, unless the runner
sets `.esp_dir(..)` or `.esp_image(..)`.

The staging directory is located at
`build/cross/<target>/<profile>/esp/` and is rebuilt whenever any of its
input files change.

## OVMF firmware

Quon probes for OVMF firmware in this order:

1. **Explicit paths** from the QEMU runner's `.ovmf(code, vars)`
2. **Environment variables:** `OVMF_CODE` and `OVMF_VARS`
3. **Firmware beside QEMU:** `share/qemu/edk2-x86_64-code.fd` and
   `edk2-i386-vars.fd` next to the `bin/` directory of the QEMU binary
   on `PATH`. This finds the firmware of a Nix or Homebrew QEMU, for
   example inside `nix shell nixpkgs#qemu`.
4. **Common system paths** (distribution-dependent)

If auto-detection fails, set the environment variables explicitly:

```sh
export OVMF_CODE=/usr/share/OVMF/OVMF_CODE.fd
export OVMF_VARS=/usr/share/OVMF/OVMF_VARS.fd
```

On macOS with Homebrew:

```sh
export OVMF_CODE="$(brew --prefix)/share/qemu/edk2-x86_64-code.fd"
export OVMF_VARS="$(brew --prefix)/share/qemu/edk2-i386-vars.fd"
```

## Complete example

The following `quon.rs` defines a project with a bare-metal kernel and
a UEFI bootloader. The bootloader embeds the kernel binary at compile time
using `artifact_env`.

```rust
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("my-os", "0.1.0");

    let kernel_target = Target::builtin("x86_64-unknown-none");
    let uefi_target = Target::builtin("x86_64-unknown-uefi");

    // Kernel: bare-metal, compiled for x86_64-unknown-none
    let kernel = q.add(
        Crate::bin("kernel", "crates/kernel")
            .target(&kernel_target)
            .linker_script("crates/kernel/kernel.ld"),
    );

    // Bootloader: a UEFI application that embeds the kernel binary
    let bootloader = q.add(
        Crate::bin("bootloader", "crates/bootloader")
            .target(&uefi_target)
            .artifact_env("KERNEL_PATH", &kernel),
    );

    q.add(Esp::new("default").add(&bootloader, "EFI/BOOT/BOOTX64.EFI"));

    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(256)
            .serial(Serial::Stdio)
            .boot(BootMode::Uefi),
    );
    q.run(&bootloader).runner(&qemu);

    Ok(())
}
```

### How artifact_env works

The `.artifact_env("KERNEL_PATH", &kernel)` call tells Quon to set the
`KERNEL_PATH` environment variable to the output path of the `kernel`
crate when compiling the `bootloader` crate. This lets the bootloader
embed the kernel at compile time:

```rust
static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));
```

Quon automatically adds a dependency edge from `bootloader` to `kernel`
in the DAG, ensuring the kernel is compiled first.

### Running

```sh
quon run
```

This compiles the kernel, compiles the bootloader (with the kernel path
injected), assembles the ESP, and launches QEMU with OVMF firmware.
The boot mode is set to UEFI on the QEMU runner, so no
additional flags are needed.
