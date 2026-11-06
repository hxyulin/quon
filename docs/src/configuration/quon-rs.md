# The `quon.rs` build script

The single configuration file for every Quon project.

A `quon.rs` is a regular Rust source file that imports the
[`quon-config`] crate and defines one function:

```rust,ignore
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()>
```

The file has no `main`. Quon generates a `main` that creates the
`Quon`, calls `config`, and prints the model as JSON. If `config`
returns an error, or a declaration reported one, Quon prints every
error and stops. Quon compiles the script when it changes (about 1 s)
and caches the binary; later runs take about 50 ms.

## Minimal example

```rust
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("my-kernel", "0.1.0");
    q.cargo_workspace(".");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    let kernel = q.add(Package::new("kernel").linker_script("crates/kernel/kernel.ld"));
    let qemu = q.add(
        Qemu::new("qemu-system-x86_64")
            .machine("q35")
            .memory_mb(128)
            .cores(1)
            .serial(Serial::Stdio),
    );
    q.run(&kernel).runner(&qemu);
    Ok(())
}
```

The `dev` and `release` profiles exist without being declared, and
build for the kernel's target. See
[Projects, Targets, and Profiles](./projects-targets-profiles.md).

## Two kinds of method

`q.add(item)` declares a named item and returns a handle to it. The
item is an owned value built with chained methods, such as
`Package::new("kernel").linker_script(..)`. Packages, crates, profiles,
runners, ESPs, images, downloads, build steps, config options, presets
and boot tests are all declared this way.

| Item | Handle `q.add` returns |
|---|---|
| `Package`, `Crate`, `Dependency`, `Download`, `Esp`, `Image`, `BuildStep` | `Artifact` |
| `Qemu`, `Command`, `Plugin` | `Runner` |
| `Profile` | `ProfileRef` |
| `opt::bool(..)`, `opt::u32(..)` and the other option functions | `OptRef<K>` |
| `Preset` | `PresetRef` |
| `BootTest` | `()` |

Other declarations take a handle by reference, so they can only refer
to something that was declared:

```rust,ignore
let kernel = q.add(Package::new("kernel").target(&none));
let boot = q.add(Package::new("bootloader").target(&uefi).artifact_env("KERNEL_PATH", &kernel));
q.add(Esp::new("boot").add(&boot, "EFI/BOOT/BOOTX64.EFI"));
```

The other methods on `Quon` set what a project has one of:

| Method | Sets |
|---|---|
| `q.project(name, version)` | The project's name and version |
| `q.cargo_workspace(dir)` | The Cargo workspace packages come from |
| `q.default_target(&target)` | The target crates build for when they name none |
| `q.default_profile(&profile)` | The profile used without `-p` or `-r` (`dev` if unset) |
| `q.run(&artifact)` | What `quon run` starts, with `.runner(&runner)` how and `.name(..)` which entry it is; see [Several run entries](../running/runners.md#several-run-entries) |
| `q.host_tests()` | The packages whose tests `quon test` runs on the host |
| `q.env(key, value)` | A variable of every rustc run and build script, as Cargo's `[env]` |
| `q.include_kconfig(path)` | A `.kconfig` file to read options and presets from |
| `q.profile(name)` | A `ProfileRef` to a profile by name, such as the built-in `dev` |

`q.cargo_workspace` is an error when called twice. A second
`q.project` replaces the first. Each `q.run` call adds a run entry, and
a project with more than one names each.

`q.env` is for a variable crates read while they compile, such as
`DEFMT_LOG`, which defmt's macros read to choose the log levels they
keep. Its value is part of every crate's command, so changing it
rebuilds every crate. A variable Quon sets for a crate itself, such as
`CARGO_PKG_NAME` or `OUT_DIR`, keeps Quon's value. Setting one name to
two values is an error. Programs that `quon run` and `quon test` start
do not get it.

### Declarations from workspace members

`q.cargo_workspace(dir).members()` returns the workspace's member
packages, sorted by name, so `quon.rs` can declare an item per package
instead of listing every one:

```rust
const BSPS: &[(&str, &str)] = &[("board-a-bsp", "cortex-m7"), ("board-b-bsp", "cortex-m4")];

for member in q.cargo_workspace(".").members() {
    if let Some(&(_, cpu)) = BSPS.iter().find(|&&(bsp, _)| member.depends_on(bsp)) {
        let firmware = q.add(Package::new(&member.name).rustc_flag(format!("-Ctarget-cpu={cpu}")));
        q.run(&firmware).name(&member.name);
    }
}
```

Each `WorkspaceMember` has the package's `name`, its `path` relative to
the workspace root with `/` between components (`.` for a package at
the root), and its `dependencies`: the package names in
`[dependencies]`, after renames. Build and dev dependencies are left
out. `depends_on(name)` checks that set.

Quon reads the manifests while the script runs: the script asks the
quon process that started it, which answers from the same loader that
builds. Quon runs the script on every command, so a new or edited
`Cargo.toml` shows up on the next command without recompiling
`quon.rs`. A workspace Quon cannot read is a config error naming the
call. Outside Quon, as in a unit test of the script with `Quon::new()`,
`members()` returns an empty list and records an error.

A `Target` is a plain value, not an item: `Target::builtin(triple)`,
`Target::spec(name, path)` or `Target::host()`. Quon registers it the
first time a declaration uses it.

## Your own declarations

`q.add` takes any type that implements the `Declare` trait. A crate,
or a module of your `quon.rs`, can implement it to offer its own typed
declaration, built from the ones Quon provides:

```rust
use quon_config::prelude::*;

/// A probe-rs runner for one chip.
struct ProbeRs { chip: String }

impl Declare for ProbeRs {
    type Handle = Runner;
    fn declare(self, q: &mut Quon) -> Runner {
        q.add(Plugin::new("probe-rs").setting("chip", self.chip))
    }
}

pub fn config(q: &mut Quon) -> Result<()> {
    let board = q.add(ProbeRs { chip: "nRF52840_xxAA".into() });
    # let _ = board;
    Ok(())
}
```

A `declare` that checks its input reports a problem with
`q.error(message)`. The script still runs to the end, so Quon reports
every error at once.

## Splitting config across files

A `quon.rs` that describes several subsystems can be split into
several files with Rust's `mod` declarations and the `#[path]`
attribute. Quon adds no API for this.

### Layout

```text
my-kernel/
├── quon.rs                  # root: project, targets, run entry
├── bootloader/
│   ├── quon.rs              # the bootloader's declarations
│   └── ...source...
└── kernel/
    ├── quon.rs              # the kernel's declarations
    └── ...source...
```

### Root `quon.rs`

```rust,ignore
use quon_config::prelude::*;

#[path = "bootloader/quon.rs"]
mod bootloader;
#[path = "kernel/quon.rs"]
mod kernel;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("my-kernel", "0.1.0");
    q.cargo_workspace(".");

    let kernel = kernel::register(q);
    let boot = bootloader::register(q, &kernel);

    let qemu = q.add(Qemu::new("qemu-system-x86_64").machine("q35").boot(BootMode::Uefi));
    q.run(&boot).runner(&qemu);
    Ok(())
}
```

### A subsystem file

```rust,ignore
// bootloader/quon.rs
use quon_config::prelude::*;

pub fn register(q: &mut Quon, kernel: &Artifact) -> Artifact {
    let boot = q.add(
        Package::new("bootloader")
            .target(&Target::builtin("x86_64-unknown-uefi"))
            .artifact_env("KERNEL_PATH", kernel),
    );
    q.add(Esp::new("boot").add(&boot, "EFI/BOOT/BOOTX64.EFI"));
    boot
}
```

### Conventions

- Each subsystem exposes a `register` function that takes `&mut Quon`
  and returns the handles other files need. This is a convention, not
  a trait.
- Files refer to each other's items by passing handles. A function
  that needs the kernel takes `&Artifact`.
- Each file can build its own `Target` value. Two values with the same
  name and definition are the same target. One name with two
  definitions, such as a builtin and a spec, is an error.
- `#[path = "..."]` is required because subsystem files don't sit at
  the default `<modname>.rs` location.

Each named item is declared once per name. Crates and build steps
share one namespace, since both are build inputs. Runners have no
name to repeat.

### Cache behaviour

The script runner hashes every file the script uses, from the depfile
rustc writes, so editing `bootloader/quon.rs` rebuilds the compiled
script the same way editing the root `quon.rs` does.

## Why Rust?

The Rhai DSL fought a dynamically typed scripting language inside a
statically typed host. With Rust, the type system catches most of
what the engine, intern and validate layers enforced at run time.
rust-analyzer provides completions, hover, go-to-definition and inline
type errors in `quon.rs`.

The trade-offs:

- **First-time compile cost.** About 1 s per script change, plus a
  one-time build of the quon-config library for each rustc.
- **Conditional logic in the script is real Rust.** `if`, `for` and
  functions are available. A build script that needs a binary search
  is probably doing too much.
- **Errors carry no source position.** They name the item at fault, as
  in `profile 'p' is declared more than once`.

## The quon-config library and `~/.quon`

The script links against a compiled quon-config library. Quon
carries the quon-config and quon-model sources inside its own binary,
so it needs neither a Quon checkout nor a crates.io release to build
the library. It unpacks them and builds them with cargo once per set of
sources and rustc, and every project shares the result. It lives in
the Quon home directory, `~/.quon`, or the directory `QUON_HOME`
names:

```text
~/.quon/
└── config-lib/<hash of the sources and rustc>/
    ├── src/             # the unpacked quon-config and quon-model sources
    ├── manifest.json    # paths of the built library files
    └── target/
```

The first command that needs the library builds it, which takes about
5 s. To build it ahead of time, run `quon setup` in the project. It
builds the library for the rustc that the directory uses, and prints
the directory:

```sh
$ quon setup
/home/me/.quon/config-lib/3f9c…
```

serde and serde_json come from crates.io, at the versions in Quon's
own `Cargo.lock`. Installing Quon put them in cargo's cache, so the
build runs offline first and usually needs no network. If the offline
build fails, Quon retries online. A rustc upgrade or a Quon with
different quon-config sources gets a new entry; delete old entries to
reclaim the space.

The compiled script itself stays in the project, under
`build/script-runner/`.

## Editor support

Run `quon configure` in the project root to generate a
`rust-project.json`. rust-analyzer picks it up and indexes your kernel
sources and `quon.rs`, with completions, hover and diagnostics for
every builder method. The file declares `quon.rs` as a library crate,
because it has no `main`, and points rust-analyzer at the quon-config
sources the library was built from.

## Migrating from `fn main`

Earlier versions had `quon.rs` define `main`, create the config value
and call `emit()` on it. Replace `main` with a `config` function that
takes `&mut Quon`:

```rust,ignore
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    // ...
    Ok(())
}
```

Pass `q` instead of `&mut q` to helper functions. A script that still
defines `main` fails to compile, and the error says how to migrate.
To move from the `Config` API to `Quon`, see
[From Config to Quon](../migration/config-to-quon.md).

[`quon-config`]: https://docs.rs/quon-config
