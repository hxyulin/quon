# Limine kernel

A kernel booted by [Limine](https://github.com/Limine-Bootloader/Limine)
over UEFI. Quon builds the kernel, fetches Limine's prebuilt
`BOOTX64.EFI` from a pinned release, and assembles an ESP that holds
both, plus the project's `limine.conf`. Quon does not generate the
Limine configuration file.

## quon.rs

```rust
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("limine-kernel", "0.1.0");
    q.cargo_workspace(".");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    let kernel = q.add(
        Package::new("kernel")
            .linker_script("crates/kernel/kernel.ld")
            .rustc_flag("-Crelocation-model=static"),
    );

    let limine = q.add(Download::limine(
        "11.4.1",
        "BOOTX64.EFI",
        "333f7a69379b1f47e019be215885cdc078b9edd6e9255cbe2f706d6c5ec0ef06",
    ));

    q.add(
        Esp::new("default")
            .add(&limine, "EFI/BOOT/BOOTX64.EFI")
            .add("limine.conf", "limine.conf")
            .add(&kernel, "boot/kernel"),
    );

    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(256)
            .serial(Serial::Stdio)
            .boot(BootMode::Uefi)
            .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
            .success_exit_code(33),
    );
    q.run(&kernel).runner(&qemu);

    Ok(())
}
```

## Pinning Limine

`Download::limine(version, file, sha256)` declares one file from the
release's `v<version>-binary` tag in the GitHub repository, as a
download named `limine/<file>`. `q.add` returns its handle, which the
ESP takes like any other artifact. A string source in `Esp::add`, such
as `"limine.conf"`, names a file in the project.
An x86_64 UEFI boot needs only `BOOTX64.EFI`.

The first build that needs the file fetches it with `curl`, checks its
SHA-256 and caches it at `build/downloads/<sha256>/BOOTX64.EFI`. Later
builds hash the cached copy and fetch again only if it no longer
matches. A hash mismatch fails the build and prints both hashes.

To upgrade, change the version and the hash in `Download::limine`.
To find the new hash, leave it empty (`""`): the build fails and prints
the hash of the file it fetched. Check where the file came from before
you pin it. Limine 8 and later read `limine.conf`; Limine 7 reads
`limine.cfg`, which has a different syntax.

`Download::new(name, url, sha256)` pins any other file the same way,
and `Image::new(..).add(&download, dest)` puts a download into a disk
image.

## Kernel requirements

- The Limine protocol loads the kernel at its link address, which must
  be in the top 2 GiB. `kernel.ld` links at `0xffffffff80000000`.
- `x86_64-unknown-none` links a position-independent executable by
  default. Limine loads one only with a `PT_DYNAMIC` segment, so the
  example links a plain executable with `-Crelocation-model=static`.
- The kernel keeps its request structures in `.limine_requests`, which
  the linker script keeps with `KEEP`.

## Running

```sh
quon run
```

Quon finds OVMF next to the QEMU binary, so `nix shell nixpkgs#qemu`
is enough on Nix. The kernel prints the bootloader's name and version
on the serial port, then writes `0x10` to the `isa-debug-exit` port.
QEMU exits with status 33, `(0x10 << 1) | 1`. The config sets
`.success_exit_code(33)`, so `quon run` exits 0. Any other status, such
as 35 from the kernel's failure path, fails the run.

## Runnable example

`examples/limine-kernel/` in the Quon repository is a runnable version
of this setup. The `limine_boot` integration test in `quon` boots
it under Limine 7.13.3, 8.7.0, 9.6.7, 10.8.5 and 11.4.1.
The kernel requests base revision 6 (the `limine` crate's default).
Limine 11 grants it. Older releases still boot the kernel at the
highest revision they support: 5 for 10.8.5, 3 for 9.6.7 and 8.7.0,
and none for 7.13.3. A real kernel should check
`BaseRevision::is_supported()`.
