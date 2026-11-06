# Testing

`quon test` runs three kinds of test, in this order:

| Kind | What Quon builds | Where it runs | Passes when |
|------|-------------------|---------------|-------------|
| Host tests | The lib and `harness = true` tests of each `cfg.host_tests()` package, with `--test`, and its doctests | On the host | The libtest executable or rustdoc exits with 0 |
| Integration tests | The `harness = false` tests in `tests/` of `group.package(...)` packages | The profile's [runner](./runners.md), one run per test | The runner exits with its success status |
| Boot tests | The run binary of the profile a `cfg.boot_test(...)` names | That profile's runner, one run per suite | The runner exits with its success status |

Host tests run first because they are the fastest and need no QEMU.
Every run happens even when an earlier one fails, and `quon test` ends
with one summary over all of them:

```text
test result: ok. passed: host 2 of 2, integration 1 of 1, boot 1 of 1
```

`--host`, `--integration` and `--boot` select kinds; with none of them,
every kind runs. The filter argument goes to each host test executable
as libtest's filter, and selects integration and boot tests by name.

## Host tests

Code that does not depend on the machine, such as parsers, allocators'
bookkeeping or page-table arithmetic, can be tested on the host with
the libtest harness and `std`. List the packages whose tests to run:

```rust,ignore
cfg.host_tests().package("acpi").package("elf");
```

For each package, Quon builds its lib and each of its bins with
`rustc --test` against the host's standard library, and each
`harness = true` test in `tests/` the same way, then runs the
executables. The package must compile for the
host. A `#![no_std]` crate usually needs `#![cfg_attr(not(test),
no_std)]` so its tests can use `std`. A package does not need to be in a
group to have host tests.

Host tests resolve like `cargo test -p <package>` on the host:

- The package's dev-dependencies apply, with its default features.
- Feature sets are separate from the kernel's and from those of build
  scripts and proc macros, so a feature the kernel or a
  build-dependency turns on does not reach the tests.
- Copies of crates built for the host tests take the suffix
  `@host-test` in the build output. The lib's unit-test executable
  takes `@test`, as in `calc@test`, and a bin's takes `@bin-test`.
- When the package has tests in `tests/`, its bins build too, and those
  tests get each bin's path as `CARGO_BIN_EXE_<name>` and a scratch
  directory, `build/tmp`, as `CARGO_TARGET_TMPDIR`.

Group settings (linker scripts, rustc flags and `cfg_flag`s) and the
config crate do not apply, since they describe the kernel target.
Optimisation and debug info come from the active profile. Build
scripts run, as under Cargo.

Each executable runs from its package's directory, with the
`CARGO_PKG_*` and `CARGO_MANIFEST_DIR` variables Cargo sets, and its
output passes through. Quon cannot run the tests of a proc-macro
package yet.

### Environment

Tests that read an environment variable at run time, the way Cargo's
`[env]` table would set it, get it from the host-test builder:

```rust,ignore
cfg.host_tests()
    .package("my-tool")
    .env("RUST_LOG", "debug")
    .env_path("MY_TOOL_HOME", "build/my-tool-home");
```

`.env_path` makes the path absolute against the project root, since each
test runs from its own package directory. Both apply to every host test
run, doctests included, when it runs. They do not reach `env!`, which
reads variables at compile time.

### Doctests

After a package's other host tests, Quon runs its doctests with
`rustdoc --test`, from the package directory. They link the package's
lib and its dev-dependencies, and see the same features, build-script
output and `OUT_DIR` as the lib, as under `cargo test`. The filter goes
to them too. A lib with `doctest = false` in its `[lib]` table gets no
doctest run. Each doctest is named by its file relative to the package,
as in `src/lib.rs - add (line 6)`.

rustdoc compiles and runs the doctests in one step, so they are not
cached: every `quon test` compiles them again, as `cargo test` does.
The build writes the rustdoc command next to the unit-test executable,
because the command depends on build-script output. `quon test
--dry-run` therefore prints a comment line in place of the doctest
command, and `--no-build` before any build reports the doctests as
failed.

## Integration tests

A kernel's integration test is a whole kernel image: its own entry point,
its own panic handler, and the kernel library linked in. `quon test`
builds each one and runs it with the profile's runner on its own, so
under QEMU a test starts from a fresh machine and may end the run, for
example by faulting on purpose. Under a QEMU runner with UEFI boot, each
test boots from its own copy of the ESP.

### Declaring a test

Tests live in `tests/` of a package named with `group.package(...)`,
the same place Cargo looks. Each needs `harness = false`, because the
libtest harness needs `std`:

```toml
[[test]]
name = "heap"
harness = false
```

Quon skips a test without `harness = false` and warns about it,
unless the package is in `cfg.host_tests()`, which runs it on the host.
The
test builds like the package's bin: it links the package's lib, its
normal dependencies and its dev-dependencies, and takes the group's
linker script, rustc flags and config crate. Quon also passes
`--cfg test`, as Cargo does.

### Dev-dependencies

A test can use the package's `[dev-dependencies]`, including
`[target.'cfg(...)'.dev-dependencies]` tables matched against the
group's target, and proc macros, which build for the host:

```toml
[dev-dependencies]
ktest = { path = "../ktest" }
```

Quon resolves features the way `cargo build` and `cargo test` do:

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

### How a test boots

`quon test` boots a test the way `quon run` boots the profile's
`run_binary`, with the test binary in its place:

- **UEFI:** Quon copies the built ESP that holds the run binary to
  `esp/<esp>.tests/<test>/` and replaces each entry whose source is the
  run binary with the test binary. `quon.rs` needs no extra setup.
- **Direct boot:** the test binary is passed as `-kernel`.
- **BIOS boot** is not supported, since the disk image is built once.

A test passes when the runner exits with its `.success_exit_code(...)`, or 0
when that is unset. A test that runs past its timeout fails. Tests run
one at a time, since they share the profile's OVMF variables file.

## Boot tests

A kernel can carry its own test runner: tests registered in the kernel
image that run after boot, selected by a config option. A boot test
suite builds a profile that turns the runner on and boots it:

```rust,ignore
cfg.config_bool("KTEST").default_value(ConfigValue::Bool(false));
cfg.preset("test").set("KTEST", ConfigValue::Bool(true));
cfg.profile("test").inherits("dev").preset("test");

cfg.boot_test("ktest").profile("test");
```

`quon test` builds the suite's profile and boots its run binary as
`quon run -p <profile>` would. The suite passes when the runner exits with
the runner's `.success_exit_code(...)`, or 0 when that is unset, within the
timeout. Quon reports the suite as one result; the kernel's own
output shows which of its tests ran. The filter selects suites by name
and does not reach the kernel.

The profile builds into its own output directory, so building it does
not evict the active profile's build.

## Which commands build tests

`quon build` and `quon run` skip test crates, the dev-dependencies,
and the `@test-pass` and `@host-test` copies, so a broken test does not
break the build. `quon check` and `quon clippy` include them, as
`cargo clippy --all-targets` does, and `quon test` builds them.
