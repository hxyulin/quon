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
