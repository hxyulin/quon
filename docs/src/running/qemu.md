# QEMU Orchestration

Quon can build your project and launch it in QEMU in a single command.
The QEMU configuration lives in your `quon.rs` alongside the rest of
the build model, so the run environment is versioned with your source code.

## Configuring QEMU

Declare a QEMU [runner](./runners.md) in your `quon.rs`, and name it
in the run entry:

```rust,ignore
let qemu = q.add(
    Qemu::new("qemu-system-x86_64")
        .machine("q35")
        .memory_mb(256)
        .cores(2)
        .serial(Serial::Stdio)
        .boot(BootMode::Direct),
);
q.run(&kernel).runner(&qemu);
```

The argument to `Qemu::new(...)` is the QEMU executable name. It must
be on your `$PATH` or be an absolute path. A project can declare
several QEMU runners, for example one per machine, and pick one per
profile with `Profile::runner(&r)`.

### Builder methods

| Method | Description |
|---|---|
| `.machine(type)` | `-machine <type>`, such as `"q35"` or `"virt"` |
| `.memory_mb(n)` | `-m <n>M` |
| `.cores(n)` | `-smp <n>` |
| `.serial(mode)` | Where the first serial port goes: `Serial::Stdio`, `Serial::None` or `Serial::File(path)` |
| `.display(mode)` | The display backend. QEMU runs headless by default |
| `.boot(mode)` | `BootMode::Direct`, `BootMode::Uefi` or `BootMode::Bios` |
| `.args([..])` | Arguments appended after every argument Quon sets |
| `.success_exit_code(n)` | Exit status that `quon run` reports as success |
| `.ovmf(code, vars)` | The OVMF firmware files for UEFI boot |
| `.esp_dir(path)`, `.esp_image(path)` | The EFI system partition, as a host directory or an image file |
| `.disk(&image)`, `.disk_file(path)` | Boot from a declared `Image`, or from a disk image file |

### Exit status

`quon run` succeeds when QEMU exits with status 0. A kernel that stops
QEMU through the `isa-debug-exit` device cannot produce 0: QEMU exits
with `(value << 1) | 1`, where `value` is what the kernel wrote to the
port. Set the status that means success:

```rust,ignore
let qemu = q.add(
    Qemu::new("qemu-system-x86_64")
        .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
        .success_exit_code(33), // the kernel writes 0x10
);
```

With `.success_exit_code(n)` set, status `n` makes `quon run` exit 0.
Every other status, 0 included, fails the run.

## Running

```sh
quon run                 # Build + launch QEMU
quon run --no-build      # Skip build, use existing artifacts
quon run --dry-run       # Print the QEMU command without executing
quon run -- -serial mon:stdio  # Pass extra args to QEMU after --
```

The `--` separator passes all subsequent arguments directly to QEMU,
overriding or supplementing the configured arguments.

## Boot mode selection

Quon resolves the boot mode using the following precedence (highest to
lowest):

1. **CLI flags:** `--uefi`, `--direct` or `--bios`
2. **Runner settings:** `.boot(BootMode::Uefi)` on the QEMU runner
3. **Default:** direct kernel boot

To boot one kernel two ways, declare two QEMU runners and give one
profile the second with `Profile::runner(&r)`. See
[What `quon run` starts](../configuration/projects-targets-profiles.md#what-quon-run-starts).

### Direct boot

QEMU loads the kernel ELF directly via `-kernel <path>`. This is the
simplest mode and requires no firmware. The kernel binary must be a
valid ELF that QEMU can load.

### UEFI boot

Uses OVMF firmware with an EFI System Partition. QEMU boots into UEFI,
which loads the bootloader from the ESP. See
[UEFI Boot and ESP Assembly](./uefi-boot.md) for setup details.

## Timeouts

```sh
quon run -T 30           # Kill QEMU after 30 seconds
```

The timeout is useful for automated testing: QEMU is terminated after the
specified number of seconds, and Quon reports whether the process exited
cleanly.
