# Dependencies

Gluon manages external crate dependencies directly, without relying on Cargo
for resolution. Dependencies are declared in `gluon.rs` and linked to
individual crates in the build model.

## Declaring external dependencies

Use the `cfg.dependency()` builder to declare an external crate:

```rust
cfg.dependency("log")
    .version("0.4");

cfg.dependency("spin")
    .version("0.9")
    .default_features(false)
    .features(&["mutex"]);

cfg.dependency("my-local-crate")
    .path("../my-local-crate");

cfg.dependency("my-git-dep")
    .git("https://github.com/user/repo")
    .branch("main");
```

### Dependency methods

| Method | Description |
|---|---|
| `.version(ver)` | crates.io version requirement (e.g., `"0.4"`, `"^1.0"`) |
| `.path(path)` | Local path dependency (relative to project root) |
| `.git(url)` | Git repository URL |
| `.branch(name)` | Git branch (used with `.git()`) |
| `.tag(name)` | Git tag (used with `.git()`) |
| `.rev(hash)` | Git revision hash (used with `.git()`) |
| `.default_features(bool)` | Enable or disable default features |
| `.features(&[...])` | Enable specific features as a slice of string references |
| `.optional(bool)` | Mark as an optional dependency |

Source specifiers (`.version()`, `.path()`, `.git()`) are mutually exclusive --
each dependency must use exactly one.

## Linking dependencies to crates

After declaring a dependency at the top level, reference it from a crate with
`.dep()`:

```rust
cfg.dependency("log").version("0.4");

let target = cfg.target("x86_64-unknown-none");

cfg.group("kernel")
    .target(target)
    .edition("2021")
    .add("kernel", "crates/kernel")
        .dep("log", "log");
```

The first argument to `.dep()` is the **alias** -- the name used in
`extern crate` or `use` statements in your Rust code. The second argument is
the **dependency name** as declared in the `cfg.dependency()` call.

These are often the same, but they can differ when you want to rename a crate:

```rust
cfg.dependency("spin").version("0.9");

let target = cfg.target("x86_64-unknown-none");

// Use as `use spinlock::Mutex;` in Rust code
cfg.group("kernel")
    .target(target)
    .edition("2021")
    .add("kernel", "crates/kernel")
        .dep("spinlock", "spin");
```

## Vendoring

Gluon can vendor external dependencies into a local `./vendor/` directory for
offline and reproducible builds:

```sh
gluon vendor              # Vendor dependencies, write gluon.lock
gluon vendor --check      # Verify vendor tree integrity without modifying
gluon vendor --force      # Re-vendor even if fingerprints match
gluon vendor --offline    # No network access (lockfile must exist)
```

### How vendoring works

1. Gluon generates a scratch `Cargo.toml` from the declared `dependency()`
   entries.
2. It runs `cargo vendor` to download and populate `./vendor/`.
3. It writes `gluon.lock`, which pins every dependency with checksums.
4. On subsequent builds, Gluon automatically registers vendored crates into the
   build model and compiles them with `rustc` alongside your project crates.

### Build scripts

A dependency built for a cross target (`.target(kernel)`) is compiled by
Gluon with `rustc`, and so is every crate in its closure. When one of those
crates has a build script (`build.rs`), Gluon compiles the script for the
host, runs it with the environment Cargo gives scripts, and applies its
`cargo:` directives (`rustc-cfg`, `rustc-env`, `rustc-link-lib`, `OUT_DIR`
and the rest) to the crate's `rustc` command.

- A script configuring a crate built for the target sees `TARGET` set to
  that target, and `CARGO_ENCODED_RUSTFLAGS` includes Gluon's sysroot, so a
  probe that compiles a test file sees the same `core` the crate links.
- Proc macros, build scripts and their dependencies are host units with
  a fixed host profile (`opt-level=0`, no debuginfo).
- A script reruns when a path it named with `rerun-if-changed`, a variable
  it named with `rerun-if-env-changed`, the script itself or the toolchain
  changes. A script that names no input reruns when any file in its package
  changes.
- Warnings from these scripts are shown with `gluon -v` only, as Cargo hides
  warnings from dependencies.

Outputs live next to the other host and target units:
`build/host/build/<package>-<hash>/` holds a compiled script, and
`build/{host,cross/<target>/<profile>}/build/<package>-<hash>/` holds a run,
with `out/` (the script's `OUT_DIR`) and `output` (its stdout).

### Version control

The `gluon.lock` file should be committed to version control. This ensures that
every clone of the repository builds against the exact same dependency versions,
regardless of what is currently published on crates.io.

The `vendor/` directory itself can optionally be committed for fully offline
builds, or left out of version control and regenerated with `gluon vendor` as
needed.
