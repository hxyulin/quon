# Migrating `quon.rhai` to `quon.rs`

Quon 0.x phases out the Rhai DSL in favor of a typed Rust build
script (`quon.rs`) that imports the
[`quon-config`](https://docs.rs/quon-config) crate. This guide
covers the mechanical translation. For the rationale, see the
[restructure spec](https://github.com/hxyulin/quon/blob/main/docs/superpowers/specs/2026-05-01-quon-restructure-spec.md#d1-replace-rhai-with-a-typed-rust-build-script).

## Quick reference

| Rhai                                          | Rust                                            |
|-----------------------------------------------|-------------------------------------------------|
| `project("name", "1.0.0");`                   | `cfg.project("name", "1.0.0");`                 |
| `project("name", "1.0.0").default_profile("dev");` | `cfg.project("name", "1.0.0").default_profile("dev");` |
| `target("x86_64-unknown-none");`              | `let t = cfg.target("x86_64-unknown-none");`    |
| `target("custom", "spec.json");`              | `let t = cfg.target_spec("custom", "spec.json");` |
| `profile("dev").target("x86_64-...")`         | `cfg.profile("dev").target(t)`                  |
| `group("g").target("host")`                   | `cfg.group("g").target_host()`                  |
| `group("g").add("crate", "path").crate_type("bin")` | `cfg.group("g").add("crate", "path").crate_type(CrateType::Bin)` |
| Multi-`add` (Rhai `let cross = group(...); cross.add(...); cross.add(...);`) | `let mut g = cfg.group(...).target(t).edition("2021"); g.add(...); g.add(...);` |
| `crate.deps(#{ ext: #{ crate: "c" } })`       | `crate.dep("ext", "c")`                         |
| `crate.deps(#{ ext: #{ crate: "c", features: ["x"] } })` | `crate.dep_with("ext", "c", \|b\| { b.features(&["x"]); })` |
| `crate.cfg_flags(["a", "b"])`                 | `crate.cfg_flag("a").cfg_flag("b")`             |
| `crate.artifact_env("K", "kernel")`           | `crate.artifact_env("K", "kernel")`             |
| `qemu("qemu").boot_mode("uefi")`              | `cfg.runner("qemu").qemu("qemu").boot_mode("uefi")`, plus `.runner("qemu")` on the profile |
| `esp("default").add("bl", "EFI/BOOT/BOOTX64.EFI")` | `cfg.esp("default").add("bl", "EFI/BOOT/BOOTX64.EFI")` |
| `dependency("d").path("../d")`                | `cfg.dependency("d").path("../d")`              |
| `config_bool("F").default_value(true)`        | `cfg.config_bool("F").default_value(ConfigValue::Bool(true))` |
| `config_u32("L").range(0, 5)`                 | `cfg.config_u32("L").range(0, 5)`               |
| `config_*.depends_on("X")`                    | `cfg.config_*.depends_on(Expr::Ident("X".into()))` |
| `config_*.depends_on(["X", "Y"])`             | `cfg.config_*.depends_on(Expr::And(vec![Expr::Ident("X".into()), Expr::Ident("Y".into())]))` |
| `config_*.depends_on_expr("A && !B")`         | (see "Boolean expressions" below)               |
| `preset("p").set("F", true)`                  | `cfg.preset("p").set("F", ConfigValue::Bool(true))` |
| `load_kconfig("./options.kconfig");`          | `cfg.include_kconfig("./options.kconfig")?;`    |

## Skeleton

Every `quon.rs` looks like this:

```rust
use quon_config::{Config, CrateType, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    // ... declarations ...
    Ok(())
}
```

Quon creates the `Config`, calls `config`, then serialises the model
as JSON to stdout, failing with the first accumulated diagnostic if
any. The quon CLI captures that JSON, runs intern + validate, and
proceeds with the build.

## Targets are handles, not strings

Rhai accepts the target *name* in every place a target is needed
(`profile.target("x86_64-unknown-none")`). Rust's typed API takes a
`Handle<TargetDef>` — bind it once and pass it around:

```rust
let target = cfg.target("x86_64-unknown-none");
cfg.profile("dev").target(target);
cfg.group("kernel").target(target);
```

This catches typos at compile time. Stage 4 reshapes the model so
*every* cross-reference goes through handles; Stage 2's surface is
the leading edge of that change.

## Multiple crates in one group

Rhai's pattern is to bind the group locally and call `.add` on it
repeatedly:

```rhai
let cross = group("kernel").target("x86_64-unknown-none").edition("2021");
cross.add("mylib", "crates/mylib");
cross.add("app", "crates/app").crate_type("bin");
```

The Rust API takes `&mut self` on `add`, so the same shape works:

```rust
let mut cross = cfg.group("kernel").target(target).edition("2021");
cross.add("mylib", "crates/mylib");
cross.add("app", "crates/app").crate_type(CrateType::Bin);
```

For groups with a single crate, the inline chain still works:

```rust
cfg.group("kernel").target(target).edition("2021")
    .add("kernel", "crates/kernel")
    .crate_type(CrateType::Bin);
```

## Boolean expressions in `depends_on`

Rhai exposes three overloads:

```rhai
config_bool("X").depends_on("A");           // single ident
config_u32("Y").depends_on(["A", "B"]);     // AND of idents
config_bool("Z").depends_on_expr("A && !B"); // string-form expression
```

The Rust DSL collapses these into one method that takes a
structural [`Expr`] value:

```rust
use quon_config::Expr;

cfg.config_bool("X").depends_on(Expr::Ident("A".into()));

cfg.config_u32("Y").depends_on(Expr::And(vec![
    Expr::Ident("A".into()),
    Expr::Ident("B".into()),
]));

cfg.config_bool("Z").depends_on(Expr::And(vec![
    Expr::Ident("A".into()),
    Expr::Not(Box::new(Expr::Ident("B".into()))),
]));
```

This is more verbose but explicit — every node is a typed Rust
value and rust-analyzer can introspect it. For projects with many
complex expressions, declare options in a `.kconfig` file and pull
it in:

```rust
cfg.include_kconfig("./options.kconfig")?;
```

The `.kconfig` format keeps the string-form expression grammar
(`A && !B`, `||`, `!`, parenthesised subexpressions) and is the
intended path for configurations that lean heavily on boolean logic.

## Diagnostics: builder-time errors

Rhai surfaces builder errors with full source spans. The Rust DSL
uses the call-site position from `Location::caller()` — sufficient
for "duplicate target name" but coarser than what Rhai produced.
Errors accumulate on the [`Config`] and are reported (one at a
time, first-failure-wins) after `config` returns.

## Editor support

Run `quon configure` to generate a `rust-project.json`.
rust-analyzer indexes the build script alongside your kernel
sources — every builder method gets autocomplete, hover docs, and
live type errors. No extra LSP setup needed.

## During the transition

Both `quon.rhai` and `quon.rs` work simultaneously while you
port. When both files exist, `quon.rs` wins — so the safest
migration is:

1. Add `quon.rs` next to your existing `quon.rhai`.
2. Run `quon build` to confirm the `.rs` form compiles and
   produces a valid model.
3. Compare build outputs across both forms (e.g., delete `build/`,
   rebuild with `.rhai`, save artifacts; rename `.rhai` out of the
   way, rebuild with `.rs`, diff).
4. Once you're confident, delete `quon.rhai`.

Stage 3 of the quon restructure removes `.rhai` support entirely.
Until then, no rush — both paths are equivalent.

[`Expr`]: https://docs.rs/quon-config/latest/quon_config/enum.Expr.html
[`Config`]: https://docs.rs/quon-config/latest/quon_config/struct.Config.html
