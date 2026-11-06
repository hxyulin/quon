# Dependencies

Quon can build external crates without a Cargo workspace. A
`Dependency` declared in `quon.rs` is a crate from crates.io, a git
repository or a local path, and a `Crate` links it with `.dep`.
Packages in a Cargo workspace take their dependencies from their
`Cargo.toml` instead; see
[Registry dependencies of a Cargo workspace](#registry-dependencies-of-a-cargo-workspace).

## Declaring external dependencies

```rust,ignore
let kernel = Target::builtin("x86_64-unknown-none");

let log = q.add(Dependency::crates_io("log", "0.4").target(&kernel));

let spin = q.add(
    Dependency::crates_io("spin", "0.9")
        .no_default_features()
        .features(["mutex"])
        .target(&kernel),
);

let local = q.add(Dependency::path("my-local-crate", "../my-local-crate"));

let git = q.add(Dependency::git("my-git-dep", "https://github.com/user/repo").branch("main"));
```

The constructor sets the source:

| Constructor | Source |
|---|---|
| `Dependency::crates_io(name, version)` | crates.io, with a version requirement such as `"0.4"` |
| `Dependency::path(name, path)` | A local directory, relative to the project root |
| `Dependency::git(name, url)` | A git repository, on its default branch |

| Method | Description |
|---|---|
| `.rev(hash)`, `.tag(name)`, `.branch(name)` | Pin a git source. On another source, an error |
| `.features([..])` | Enable features |
| `.no_default_features()` | Leave the `default` feature off |
| `.cfg(flag)` | Pass `--cfg <flag>` to rustc |
| `.rustc_flag(flag)` | Pass `flag` to rustc |
| `.target(&t)` | Build for `t` instead of the host |

A dependency builds for one target, the host unless `.target(..)`
names another. To use the same crate on two targets, declare it twice
under two names.

## Linking dependencies to crates

`q.add` returns an `Artifact` handle. Pass it to `.dep` on a `Crate`:

```rust,ignore
let kernel_target = Target::builtin("x86_64-unknown-none");
let log = q.add(Dependency::crates_io("log", "0.4").target(&kernel_target));

q.add(Crate::bin("kernel", "crates/kernel").target(&kernel_target).dep(&log));
```

The crate uses the dependency under its name, with `-` replaced by
`_`. `Dep::on(&handle).rename(name)` picks another name:

```rust,ignore
let spin = q.add(Dependency::crates_io("spin", "0.9").target(&kernel_target));

// Use as `use spinlock::Mutex;` in Rust code
q.add(
    Crate::bin("kernel", "crates/kernel")
        .target(&kernel_target)
        .dep(Dep::on(&spin).rename("spinlock")),
);
```

See [Dependencies between crates](./packages-and-crates.md#dependencies-between-crates)
for the other `Dep` options.

## Vendoring

Quon can vendor external dependencies into a local `./vendor/` directory for
offline and reproducible builds:

```sh
quon vendor              # Vendor dependencies, write quon.lock
quon vendor --check      # Verify vendor tree integrity without modifying
quon vendor --force      # Re-vendor even if fingerprints match
quon vendor --offline    # No network access (lockfile must exist)
```

### How vendoring works

1. Quon generates a scratch `Cargo.toml` from the declared `Dependency`
   items.
2. It runs `cargo vendor` to download and populate `./vendor/`.
3. It writes `quon.lock`, which pins every dependency with checksums.
4. On subsequent builds, Quon automatically registers vendored crates into the
   build model and compiles them with `rustc` alongside your project crates.

### Build scripts

A dependency built for a cross target (`.target(&kernel)`) is compiled by
Quon with `rustc`, and so is every crate in its closure. When one of those
crates has a build script (`build.rs`), Quon compiles the script for the
host, runs it with the environment Cargo gives scripts, and applies its
`cargo:` directives (`rustc-cfg`, `rustc-env`, `rustc-link-lib`, `OUT_DIR`
and the rest) to the crate's `rustc` command.

- A script configuring a crate built for the target sees `TARGET` set to
  that target, and `CARGO_ENCODED_RUSTFLAGS` includes Quon's sysroot, so a
  probe that compiles a test file sees the same `core` the crate links.
- Proc macros, build scripts and their dependencies are host units with
  a fixed host profile (`opt-level=0`, no debuginfo).
- A script reruns when a path it named with `rerun-if-changed`, a variable
  it named with `rerun-if-env-changed`, the script itself or the toolchain
  changes. A script that names no input reruns when any file in its package
  changes.
- Warnings from these scripts are shown with `quon -v` only, as Cargo hides
  warnings from dependencies.

Outputs live next to the other host and target units:
`build/host/build/<package>-<hash>/` holds a compiled script, and
`build/{host,cross/<target>/<profile>}/build/<package>-<hash>/` holds a run,
with `out/` (the script's `OUT_DIR`) and `output` (its stdout).

### Version control

The `quon.lock` file should be committed to version control. This ensures that
every clone of the repository builds against the exact same dependency versions,
regardless of what is currently published on crates.io.

The `vendor/` directory itself can optionally be committed for fully offline
builds, or left out of version control and regenerated with `quon vendor` as
needed.

## Registry dependencies of a Cargo workspace

Packages declared with `q.cargo_workspace(dir)` and `Package::new(name)`
take their dependencies from their `Cargo.toml`. Registry dependencies
use the versions in the workspace's `Cargo.lock`, which Cargo writes.

By default, Quon reads their sources from Cargo's registry cache. When
`Cargo.lock` changes, Quon runs `cargo fetch --locked` and then `cargo
metadata --locked --offline` to learn where each package lives. It caches
the result in `build/cargo-sources.json`. A build with an unchanged
`Cargo.lock` and `CARGO_HOME` runs no Cargo command. Quon never edits
`Cargo.lock`.

To build offline or from an archived tree, vendor the sources:

```sh
quon vendor
```

This runs `cargo vendor --versioned-dirs --locked vendor` on the
workspace and writes `vendor/.quon-vendor-stamp`, which holds the
SHA-256 of `Cargo.lock`. Then name the directory in `quon.rs`:

```rust,ignore
q.cargo_workspace(".").vendored("vendor");
```

A build then reads `vendor/<name>-<version>/`, runs no Cargo command, and
fails if `Cargo.lock` changed since the last `quon vendor`. A `vendor/`
directory that `quon.rs` does not name is ignored.

Quon compiles registry packages with `--cap-lints allow`, as Cargo does.
Their sources in Cargo's registry cache are never checked for edits.
Sources in a vendor directory are checked like project sources, so a
patched vendored crate rebuilds.

### Git dependencies

A git dependency in a `Cargo.toml` builds from Cargo's checkout of the
commit `Cargo.lock` records, under `$CARGO_HOME/git/checkouts`. Quon
finds the checkout the way it finds registry sources: `cargo fetch
--locked`, then `cargo metadata --locked --offline`, cached in
`build/cargo-sources.json`.

- Every package of one checkout has the git source, including the path
  dependencies between them, as in Cargo. They inherit `workspace =
  true` fields from the checkout's own workspace root.
- The lock entry must have been written for the manifest's `rev`,
  `branch` or `tag`. After changing one, run `cargo update -p <name>`,
  as Cargo's `--locked` also requires.
- A git package's identity is its repository and commit, never a path,
  so the build directory and the checkout can both move.
- Quon compiles git packages with `--cap-lints allow`, as Cargo does,
  and checks their sources for edits like project sources.
- A vendor directory holds no git packages: a build with
  `.vendored(..)` reports a git dependency it reaches.

Like any dependency, a git dependency is an error only when a build
reaches it.

### Path dependencies outside the workspace

A path dependency outside the workspace, such as a crate in another
repository's workspace, inherits `workspace = true` fields from its own
workspace root, as under Cargo. That root is the nearest directory at or
above the package whose `Cargo.toml` has a `[workspace]` table. Its
`[workspace.package]`, `[workspace.dependencies]` and `[workspace.lints]`
apply; its `Cargo.lock`, resolver and `[patch]` do not. Registry versions
come from your workspace's `Cargo.lock`. A path package with no
enclosing workspace cannot use `workspace = true`.

Quon passes a path package's `[lints]` to rustc, as Cargo does, and does
not cap them.

### Build scripts and proc macros of workspace packages

Quon runs the build scripts of workspace packages and of their registry
dependencies, as described in [Build scripts](#build-scripts) above.
Proc macros, build scripts and their dependencies build for the host with
the fixed host profile. A package needed both by a proc macro and by a
target crate builds once for each.

Differences from vendored `Dependency` crates:

- Quon shows warnings from the scripts of path packages on every build.
  Warnings from registry packages' scripts appear only with `quon -v`.
- `rustc-link-arg` applies to the package's bins, so a kernel can set its
  linker script from `build.rs`:

  ```rust,ignore
  let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
  println!("cargo::rustc-link-arg=-T{dir}/kernel.ld");
  ```

- Two packages with the same `links` value in one target, or on the
  host, are an error.
- `Package::raw_binary()` needs a bin target in the package's
  `Cargo.toml`.
