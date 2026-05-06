# The `gluon.rs` build script

The single configuration file for every Gluon project.

A `gluon.rs` is a regular Rust source file that imports the
[`gluon-config`] crate, constructs a `Config` value, and emits it as
JSON to stdout via `cfg.emit()`. Gluon compiles the script on first
invocation (~5 s) and caches the binary; subsequent runs are ~50 ms.

## Minimal example

```rust
fn main() -> gluon_config::Result<()> {
    let mut cfg = gluon_config::Config::new();

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

    cfg.emit()
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

fn main() -> Result<()> {
    let mut cfg = Config::new();
    cfg.project("my-kernel", "0.1.0");

    let target = cfg.target("x86_64-unknown-none");
    cfg.profile("dev").target(target).opt_level(0).debug_info(true);
    cfg.qemu("qemu-system-x86_64").machine("q35").memory(128);

    shared::register(&mut cfg)?;
    bootloader::register(&mut cfg)?;
    kernel::register(&mut cfg)?;

    cfg.emit()
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

- **First-time compile cost.** ~5 s on a fresh build dir; cached after
  that.
- **Conditional logic in the script is real Rust.** `if`, `for`,
  arbitrary functions are available — more rope than Rhai. Use it
  judiciously; a build script that needs a binary search is probably
  doing too much.
- **Source spans for runtime errors are coarser.** Rhai had per-token
  spans; Rust gives us `Location::caller()` for the call site of a
  builder method, which is enough for "duplicate target name" but not
  for "invalid value passed inside a deeply nested expression".

## How distribution works

Stage 1 builds `gluon-config.rlib` on demand into
`<build_dir>/script-runner/rlib/<key>/`. The cache is keyed on rustc
version, gluon-config and gluon-model source contents, and `Cargo.lock`.
Subsequent builds skip the cargo invocation when the key matches.

If you've installed gluon outside its source workspace and the harness
can't find the gluon-config sources, set `GLUON_CONFIG_SOURCE_DIR` to
the gluon source checkout. (Stage 1.7 follow-up: install-time pre-built
rlib distribution.)

## Editor support

Run `gluon configure` in the project root to generate a
`rust-project.json`. rust-analyzer will pick it up and start indexing
your kernel sources alongside `gluon.rs` itself — every builder method
gets autocomplete, hover, and live diagnostics.

[`gluon-config`]: https://docs.rs/gluon-config
