# Integration tests

A kernel's integration test is a whole kernel image: its own entry point,
its own panic handler, and the kernel library linked in. `gluon test`
builds each one and boots it under QEMU on its own, so a test starts
from a fresh machine and may end the run, for example by faulting on
purpose.

## Declaring a test

Tests live in `tests/` of a package named with `group.package(...)`,
the same place Cargo looks. Each needs `harness = false`, because the
libtest harness needs `std`:

```toml
[[test]]
name = "heap"
harness = false
```

Gluon skips a test without `harness = false` and warns about it. The
test builds like the package's bin: it links the package's lib, its
normal dependencies and its dev-dependencies, and takes the group's
linker script, rustc flags and config crate. Gluon also passes
`--cfg test`, as Cargo does.

## Dev-dependencies

A test can use the package's `[dev-dependencies]`, including
`[target.'cfg(...)'.dev-dependencies]` tables matched against the
group's target, and proc macros, which build for the host:

```toml
[dev-dependencies]
ktest = { path = "../ktest" }
```

Gluon resolves features the way `cargo build` and `cargo test` do:

- The kernel's resolve ignores dev-dependencies, so the kernel binary is
  the same whether or not tests or dev-dependencies are declared.
- The tests' resolve adds the dev-dependencies of the packages that
  have tests, and their features unify with the normal dependencies'.

When a dev-dependency turns on a feature of a crate the kernel also
uses, that crate builds twice: once for the kernel and once, with the
extra feature, for the tests. The second copy takes the suffix
`@test-pass` in the build output, as does each crate that depends on
it, such as the package's lib. Crates the dev-dependencies leave
unchanged build once.

Only the dev-dependencies of a package named with `group.package(...)`
apply. Those of its dependencies never do, as in Cargo.

A test file defines what the kernel binary defines, such as the entry
symbol and the `#[panic_handler]`, and reports its result through the
exit status, for example with QEMU's `isa-debug-exit` device:

```rust,ignore
#![no_std]
#![no_main]

// The entry symbol the bootloader jumps to, as in the kernel binary.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    // The kernel library boots and then calls `main`.
    unsafe { kernel::start(main) }
}

fn main() {
    assert_eq!(2 + 2, 4);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    kernel::panic::handle(info)
}
```

## How a test boots

`gluon test` boots a test the way `gluon run` boots the profile's
`boot_binary`, with the test binary in its place:

- **UEFI:** Gluon copies the built ESP that holds the boot binary to
  `esp/<esp>.tests/<test>/` and replaces each entry whose source is the
  boot binary with the test binary. `gluon.rs` needs no extra setup.
- **Direct boot:** the test binary is passed as `-kernel`.
- **BIOS boot** is not supported, since the disk image is built once.

A test passes when QEMU exits with `qemu().success_exit_code(...)`, or 0
when that is unset. A test that runs past its timeout fails. Tests run
one at a time, since they share the profile's OVMF variables file.

## Which commands build tests

`gluon build` and `gluon run` skip test crates, the dev-dependencies
and the `@test-pass` copies, so a broken test does not break the build. `gluon check` and `gluon clippy` include them, as
`cargo clippy --all-targets` does, and `gluon test` builds them.
