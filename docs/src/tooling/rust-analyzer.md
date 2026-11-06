# rust-analyzer

## Why is this needed?

Quon does not use `Cargo.toml`, so rust-analyzer has no project model out of
the box. Without a project model, the language server cannot resolve crates,
dependencies, or target-specific `cfg` flags -- meaning no completions, no
go-to-definition, and spurious error highlights everywhere.

The `quon configure` command generates a `rust-project.json` file that gives
rust-analyzer everything it needs to understand your project.

## Generating rust-project.json

```sh
quon configure
```

This creates `rust-project.json` in your project root. To write it elsewhere:

```sh
quon configure --output ./ide/rust-project.json
```

Re-run `quon configure` whenever you:

- Add or remove crates in `quon.rs`
- Change dependencies between crates
- Change target triples or `cfg` flags
- Modify features or config options
- Run `quon build` for the first time, or change a build script

## What it includes

The generated `rust-project.json` contains:

- **Every crate** the build compiles, with its root module path: crates from
  `quon.rs`, Cargo workspace members, and registry, vendored and
  `cfg.dependency()` packages. Registry packages point at their sources under
  `$CARGO_HOME/registry/src`. A package built for both host and target appears
  twice, once per platform.
- **Dependency relationships** between crates (including the generated config
  crate if `.config(true)` is set on a group)
- **The toolchain sysroot and its library sources**, so rust-analyzer resolves
  `core` and `alloc` and can start its proc-macro server
- **`--cfg` flags and features** for each crate, matching what `quon build`
  would pass to `rustc`
- **Proc-macro dylibs** (`proc_macro_dylib_path`), the paths `quon build`
  writes them to
- **Build-script output**: the `rustc-cfg` values in `cfg`, and `OUT_DIR` and
  the `rustc-env` values in `env`. `OUT_DIR` is also an include directory, so
  `include!(concat!(env!("OUT_DIR"), "/gen.rs"))` resolves.
- **The auto-generated config crate** -- if it has not been built yet, `quon
  configure` writes a stub `lib.rs` so the language server can at least parse
  the dependency graph

## Proc macros and build scripts need one build

`quon configure` does not compile proc macros or run build scripts. It reads
what the last `quon build` left behind:

- A proc macro's `proc_macro_dylib_path` is always written, but the file exists
  only after a build. Until then rust-analyzer reports the macro as
  unexpanded.
- Build-script cfgs, `OUT_DIR` and `rustc-env` come from the build script's
  fingerprint record in `build/.fingerprint/`. Before the first build there is
  no record, and these fields are left out.

So after cloning a project, run `quon build` once, then `quon configure`.
Cargo runs build scripts during `cargo check` for the same reason; Quon keeps
`quon configure` fast and leaves running them to the build.

The record is read as it is, without a freshness check. After you change a
build script or its inputs, rust-analyzer sees the old output until you run
`quon build` and `quon configure` again.

`cfg.dependency()` crates appear only once `quon vendor` has populated their
workspace. Before that, `quon configure` leaves them out instead of failing.

## Editor configuration

Most editors with rust-analyzer support will automatically detect
`rust-project.json` in the project root. If your editor does not pick it up
automatically, see [Editor Setup](./editor-setup.md) for per-editor
instructions.

## Troubleshooting

**Derives and attribute macros show "unexpanded" errors** -- Run `quon build`
so the proc-macro dylibs exist, then re-run `quon configure` and restart the
language server.

**rust-analyzer shows "failed to find sysroot"** -- Run `quon build` at least
once so the sysroot is compiled, then re-run `quon configure`.

**Completions are missing for config constants** -- The config crate is
generated during `quon build`. After building, re-run `quon configure` and
restart the language server.

**Changes to `quon.rs` are not reflected** -- `rust-project.json` is a
snapshot. Re-run `quon configure` after editing the build script, then restart
the language server.
