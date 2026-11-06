# The `gluon.rs` build script

The single configuration file for every Gluon project.

A `gluon.rs` is a regular Rust source file that imports the
[`gluon-config`] crate and defines one function:

```rust
pub fn config(cfg: &mut gluon_config::Config) -> gluon_config::Result<()>
```

The file has no `main`. Gluon generates a `main` that creates the
`Config`, calls `config`, and prints the model as JSON. If `config`
returns an error, Gluon reports it and stops. Gluon compiles the script
when it changes (about 1 s) and caches the binary; later runs take
about 50 ms.

## Minimal example

```rust
pub fn config(cfg: &mut gluon_config::Config) -> gluon_config::Result<()> {
    cfg.project("my-kernel", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");

    cfg.profile("dev")
        .target(target)
        .opt_level(0)
        .debug_info(true)
        .boot_binary("kernel");

    cfg.group("kernel")
        .target(target)
        .edition("2024")
        .add("kernel", "crates/kernel")
            .crate_type(gluon_config::CrateType::Bin)
            .root("src/main.rs");

    cfg.qemu("qemu-system-x86_64")
        .machine("q35")
        .memory(128)
        .cores(1)
        .serial_stdio();

    Ok(())
}
```

## Splitting config across files

Beyond a couple of subsystems, a single `gluon.rs` becomes hard to
navigate. You can split the script across multiple files using Rust's
`mod` declarations with the `#[path]` attribute — no new gluon API,
no macro layer.

### Layout

```text
my-kernel/
├── gluon.rs                  # root: project, targets, profiles, qemu
├── bootloader/
│   ├── gluon.rs              # bootloader subsystem registration
│   └── ...source...
├── kernel/
│   ├── gluon.rs              # kernel subsystem registration
│   └── ...source...
└── shared/
    ├── gluon.rs              # shared crates registration
    └── ...source...
```

### Root `gluon.rs`

```rust
use gluon_config::{Config, Result};

#[path = "bootloader/gluon.rs"]
mod bootloader;
#[path = "kernel/gluon.rs"]
mod kernel;
#[path = "shared/gluon.rs"]
mod shared;

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("my-kernel", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");
    cfg.profile("dev").target(target).opt_level(0).debug_info(true);
    cfg.qemu("qemu-system-x86_64").machine("q35").memory(128);

    shared::register(cfg)?;
    bootloader::register(cfg)?;
    kernel::register(cfg)?;

    Ok(())
}
```

### A subsystem file

```rust
// bootloader/gluon.rs
use gluon_config::{Config, CrateType, Result};

pub fn register(cfg: &mut Config) -> Result<()> {
    let target = cfg.target("x86_64-unknown-none");

    cfg.group("bootloader")
        .target(target)
        .edition("2024")
        .add("bios-boot-sector", "bootloader/bios-boot-sector")
            .crate_type(CrateType::RawBinary)
            .root("src/main.rs")
            .linker_script("bootloader/bios-boot-sector/link.ld");

    cfg.image("bios.img").raw_write_crate("bios-boot-sector", 0);
    Ok(())
}
```

### Conventions

- Each subsystem exposes `pub fn register(cfg: &mut Config) -> Result<()>`.
  Convention only — not enforced by a trait or macro.
- Cross-subsystem references work by name, not by passing handles.
  `cfg.target("x86_64-unknown-none")` is idempotent: the second call from
  any subsystem returns the same handle as the first.
- `#[path = "..."]` is required because subsystem files don't sit at the
  default `<modname>.rs` location.

### Where each kind of declaration lives

| Concern              | Lives where                          |
| -------------------- | ------------------------------------ |
| `cfg.project(...)`   | Root only (project-wide).            |
| `cfg.target(...)`    | Root, OR re-lookup in a subsystem.   |
| `cfg.target_spec()`  | Root (the path is project-relative). |
| `cfg.dependency(...)`| Root (shared across subsystems).     |
| `cfg.profile(...)`   | Root (project-wide concept).         |
| `cfg.image(...)`     | Root or subsystem (boot images often subsystem-owned). |
| `cfg.qemu(...)`      | Root only.                           |
| `cfg.group(...)`     | Per-subsystem (the natural unit).    |
| `cfg.build_step(...)`| Per-subsystem when local; root when cross-cutting. |

These are recommended placements, not the only legal ones — the
`Config` API permits the same builder calls in any module, subject to
gluon's normal duplicate-name rules (each `group` / `image` / `profile` /
`dependency` / `build_step` once per name; `qemu` and `bootloader` are
singletons; `project` is last-call-wins; `target` and `target_spec` are
idempotent on identical re-declarations).

### Cache behaviour

The script-runner hashes every file the script transitively touches
(via the rustc-emitted depfile), so editing `bootloader/gluon.rs`
invalidates the compiled script binary the same way editing the root
`gluon.rs` would. You don't need to do anything to opt in.

## Why Rust?

The Rhai DSL was carrying the cost of fighting a dynamic-typed scripting
language inside a statically-typed host. Going Rust eliminates the
engine, intern, and validate runtime layers — the type system catches
most of what they enforced — and editor support strictly improves:
rust-analyzer provides completions, hover, goto-def, and inline type
errors against `gluon.rs` for free.

The trade-offs:

- **First-time compile cost.** About 1 s per script change, plus a
  one-time build of the gluon-config library for each rustc.
- **Conditional logic in the script is real Rust.** `if`, `for`,
  arbitrary functions are available — more rope than Rhai. Use it
  judiciously; a build script that needs a binary search is probably
  doing too much.
- **Source spans for runtime errors are coarser.** Rhai had per-token
  spans; Rust gives us `Location::caller()` for the call site of a
  builder method, which is enough for "duplicate target name" but not
  for "invalid value passed inside a deeply nested expression".

## The gluon-config library and `~/.gluon`

The script links against a compiled gluon-config library. Gluon builds
it with cargo once per Gluon version and rustc, and every project
shares it. It lives in the Gluon home directory, `~/.gluon`, or the
directory `GLUON_HOME` names:

```text
~/.gluon/
└── <gluon version>/config-lib/<rustc hash>/
    ├── Cargo.toml       # depends on gluon-config = "=<gluon version>"
    ├── Cargo.lock       # written by the first build, then left as is
    ├── manifest.json    # paths of the built library files
    └── target/
```

The first command that needs the library builds it, which takes about
5 to 15 s. To build it ahead of time, run `gluon setup` in the project.
It builds the library for the rustc that the directory uses, and
prints the directory:

```sh
$ gluon setup
/home/me/.gluon/0.1.1/config-lib/3f9c…
```

The build runs offline first. `cargo install gluon-cli` has already
downloaded gluon-config's sources, so it usually needs no network. If
the offline build fails, Gluon retries online. A rustc upgrade gets a
new entry; delete old entries to reclaim the space.

A `gluon` built from a Gluon checkout builds the library from that
checkout's sources instead, into `~/.gluon/dev/config-lib/<hash>/`.
The hash covers the gluon-config and gluon-model sources, so an edit to
them builds a new entry. `GLUON_CONFIG_SOURCE_DIR` points Gluon at a
checkout explicitly.

The compiled script itself stays in the project, under
`build/script-runner/`.

## Editor support

Run `gluon configure` in the project root to generate a
`rust-project.json`. rust-analyzer will pick it up and start indexing
your kernel sources alongside `gluon.rs` itself — every builder method
gets autocomplete, hover, and live diagnostics. The file declares
`gluon.rs` as a library crate, because it has no `main`, and points
rust-analyzer at the gluon-config sources the library was built from.

## Migrating from `fn main`

Earlier versions had `gluon.rs` define `main`, create the `Config` and
call `cfg.emit()`. Rewrite

```rust
fn main() -> Result<()> {
    let mut cfg = Config::new();
    // ...
    cfg.emit()
}
```

as

```rust
pub fn config(cfg: &mut Config) -> Result<()> {
    // ...
    Ok(())
}
```

and pass `cfg` instead of `&mut cfg` to helper functions. A script
that still defines `main` fails to compile, and the error says how to
migrate.

[`gluon-config`]: https://docs.rs/gluon-config
