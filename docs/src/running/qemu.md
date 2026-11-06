# QEMU Orchestration

Quon can build your project and launch it in QEMU in a single command.
The QEMU configuration lives in your `quon.rs` alongside the rest of
the build model, so the run environment is versioned with your source code.

## Configuring QEMU

Declare a QEMU [runner](./runners.md) in your `quon.rs`, and name it
in the profile:

```rust,ignore
cfg.runner("qemu")
    .qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(256)
    .cores(2)
    .serial_stdio()
    .boot_mode("direct");

cfg.profile("dev").target(x86_64).run_binary("kernel").runner("qemu");
```

The argument to `.qemu(...)` is the QEMU executable name. It must be on
your `$PATH` or specified as an absolute path. A project can declare
several QEMU runners, for example one per machine, and pick one per
profile.

### Builder methods

| Method              | Description                                     |
|---------------------|-------------------------------------------------|
| `.machine(type)`    | QEMU machine type (e.g., `"q35"`, `"virt"`)    |
| `.memory(mib)`      | RAM in MiB                                      |
| `.cores(n)`         | Number of virtual CPUs                          |
| `.serial_stdio()`   | Redirect the serial port to the terminal        |
| `.boot_mode(mode)`  | `"direct"` (default) or `"uefi"`                |
| `.extra_args(list)` | Additional QEMU arguments passed verbatim       |
| `.success_exit_code(n)` | Exit status that `quon run` reports as success |

### Exit status

`quon run` succeeds when QEMU exits with status 0. A kernel that stops
QEMU through the `isa-debug-exit` device cannot produce 0: QEMU exits
with `(value << 1) | 1`, where `value` is what the kernel wrote to the
port. Set the status that means success:

```rust,ignore
cfg.runner("qemu")
    .qemu("qemu-system-x86_64")
    .extra_args(&["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
    .success_exit_code(33); // the kernel writes 0x10
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

1. **CLI flags:** `--uefi` or `--direct`
2. **Profile settings:** `profile.qemu_boot_mode("uefi")`
3. **Runner settings:** `.boot_mode("uefi")` on the QEMU runner
4. **Default:** direct kernel boot

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

The timeout can also be set per-profile in `quon.rs`:

```rust
cfg.profile("test")
    .inherits("dev")
    .test_timeout(60);
```

## Profile overrides

Profiles can override QEMU settings to tailor the run environment for
different use cases:

```rust
cfg.profile("test")
    .inherits("dev")
    .qemu_memory(512)
    .qemu_cores(4)
    .qemu_extra_args(&["-display", "none"])
    .test_timeout(60);
```

This is particularly useful for CI, where you might want to disable the
display and set a hard timeout, or for performance testing, where you
might want more cores and memory than the default.
