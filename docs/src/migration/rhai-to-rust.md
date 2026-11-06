# Migrating `quon.rhai` to `quon.rs`

Quon no longer reads `quon.rhai`. A project describes its build in
`quon.rs`, a typed Rust build script that imports the
[`quon-config`](https://docs.rs/quon-config) crate. This guide covers
the mechanical translation. For the rationale, see the
[restructure spec](https://github.com/hxyulin/quon/blob/main/docs/superpowers/specs/2026-05-01-quon-restructure-spec.md#d1-replace-rhai-with-a-typed-rust-build-script).

## Quick reference

| Rhai | Rust |
|---|---|
| `project("name", "1.0.0");` | `q.project("name", "1.0.0");` |
| `project(..).default_profile("debug");` | `q.default_profile(&debug);`, with `debug` from `q.add(Profile::new("debug"))` |
| `target("x86_64-unknown-none");` | `let t = Target::builtin("x86_64-unknown-none");` |
| `target("custom", "spec.json");` | `let t = Target::spec("custom", "spec.json");` |
| `profile("dev").target("x86_64-...")` | `q.default_target(&t);`, or `q.run(&artifact)` on a crate of that target |
| `profile("dev").run_binary("kernel")` | `q.run(&kernel);` |
| `group("g").target("host")` | `.target(&Target::host())` on each package or crate |
| `group("g").add("crate", "path").crate_type("bin")` | `q.add(Crate::bin("crate", "path").target(&t))` |
| `crate.deps(#{ ext: #{ crate: "c" } })` | `.dep(Dep::on(&c).rename("ext"))` |
| `crate.deps(#{ ext: #{ crate: "c", features: ["x"] } })` | `.dep(Dep::on(&c).rename("ext").features(["x"]))` |
| `crate.cfg_flags(["a", "b"])` | `.cfg("a").cfg("b")` |
| `crate.artifact_env("K", "kernel")` | `.artifact_env("K", &kernel)` |
| `qemu("qemu").boot_mode("uefi")` | `let qemu = q.add(Qemu::new("qemu").boot(BootMode::Uefi));` and `q.run(&k).runner(&qemu);` |
| `esp("default").add("bl", "EFI/BOOT/BOOTX64.EFI")` | `q.add(Esp::new("default").add(&bl, "EFI/BOOT/BOOTX64.EFI"));` |
| `dependency("d").path("../d")` | `q.add(Dependency::path("d", "../d"))` |
| `config_bool("F").default_value(true)` | `q.add(opt::bool("F").default(true))` |
| `config_u32("L").range(0, 5)` | `q.add(opt::u32("L").range(0..=5))` |
| `config_*.depends_on("X")` | `.depends_on(&x)` |
| `config_*.depends_on(["X", "Y"])` | `.depends_on(&x & &y)` |
| `config_*.depends_on_expr("A && !B")` | `.depends_on(&a & !&b)` |
| `preset("p").set("F", true)` | `q.add(Preset::new("p").set(&f, true))` |
| `load_kconfig("./options.kconfig");` | `q.include_kconfig("options.kconfig");` |

Where Rhai names an item by string, Rust passes the handle that
`q.add` returned for it. In the table, `kernel`, `bl`, `c`, `x`, `a`
and `f` are such handles.

## Skeleton

Every `quon.rs` looks like this:

```rust
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    // ... declarations ...
    Ok(())
}
```

Quon creates the `Quon`, calls `config`, then prints the model as
JSON on stdout. If any declaration reported an error, it prints every
error instead and fails. The quon CLI reads that JSON, runs intern and
validate, and proceeds with the build.

## Items are handles, not strings

Rhai accepts the *name* of a target, crate or option in every place
one is needed. In Rust, `q.add` returns a handle, and other
declarations take the handle:

```rust,ignore
let kernel = q.add(Package::new("kernel").target(&none));
let boot = q.add(Package::new("bootloader").target(&uefi).artifact_env("KERNEL_PATH", &kernel));
q.add(Esp::new("default").add(&boot, "EFI/BOOT/BOOTX64.EFI"));
```

A reference to something that was never declared does not compile.
A `Target` is a plain value: build it where you need it, or bind it
once and pass `&t`.

## Groups

Rhai groups crates that share a target:

```rhai
let cross = group("kernel").target("x86_64-unknown-none").edition("2021");
cross.add("mylib", "crates/mylib");
cross.add("app", "crates/app").crate_type("bin");
```

The Rust API has no groups. Each crate carries its own target and
edition. Set the common target once with `q.default_target`:

```rust,ignore
q.default_target(&Target::builtin("x86_64-unknown-none"));
let mylib = q.add(Crate::lib("mylib", "crates/mylib").edition(Edition::E2021));
q.add(Crate::bin("app", "crates/app").edition(Edition::E2021).dep(&mylib));
```

For a Cargo workspace, declare `Package::new(name)` instead. Its
edition, dependencies and crate types come from its `Cargo.toml`.

## Boolean expressions in `depends_on`

Rhai exposes three overloads:

```rhai
config_bool("X").depends_on("A");           // single ident
config_u32("Y").depends_on(["A", "B"]);     // AND of idents
config_bool("Z").depends_on_expr("A && !B"); // string-form expression
```

In Rust, `.depends_on` takes a condition built from option handles
with `&`, `|` and `!`:

```rust,ignore
let a = q.add(opt::bool("A"));
let b = q.add(opt::bool("B"));

q.add(opt::bool("X").depends_on(&a));
q.add(opt::u32("Y").depends_on(&a & &b));
q.add(opt::bool("Z").depends_on(&a & !&b));
```

An option declared in a `.kconfig` file has no handle. Refer to it
with `Cond::named("A")`. The `.kconfig` format keeps the string-form
grammar (`A && !B`, `||`, `!`, parenthesised subexpressions):

```rust,ignore
q.include_kconfig("options.kconfig");
```

## Diagnostics

Rhai surfaced builder errors with source spans. The Rust API reports
errors without a source position. Each names the item at fault, such
as `profile 'p' is declared more than once`. The script runs to the
end, and Quon reports every error at once after `config` returns.

## Editor support

Run `quon configure` to generate a `rust-project.json`.
rust-analyzer indexes the build script alongside your kernel
sources, with completions, hover docs and type errors for every
builder method.

## Porting a project

1. Write `quon.rs` next to `quon.rhai`, using the table above.
2. Run `quon build` to check that it compiles and produces a valid
   model.
3. Delete `quon.rhai`. Quon ignores it.
