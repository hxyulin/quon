# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed (`quon.rs` API)

- **`quon.rs` fills in a `Quon` instead of a `Config`.** The script is
  `pub fn config(q: &mut Quon) -> Result<()>`, with
  `use quon_config::prelude::*;`. A script written for `Config` fails
  to compile, and Quon prints a note with the new form.
- **Items are values passed to `q.add`**, which returns a typed handle:
  `q.add(Package::new("kernel"))` returns an `Artifact`, and
  `q.add(Qemu::new(..))` a `Runner`. Other declarations take handles,
  not name strings.
- **Groups are removed.** Each `Package` and `Crate` carries its own
  target, and `q.default_target(&t)` sets it for the rest.
- **`dev` and `release` are built in.** Profiles hold opt-level, debug
  info, LTO, a preset and an optional runner override.
  `q.run(&artifact).runner(&runner)` names what `quon run` starts, and
  profiles build for that artifact's target.
- `cfg.rule(..)`, `cfg.bootloader(..)` and
  `Profile::qemu_boot_mode` are removed. See
  [`docs/src/migration/config-to-quon.md`](docs/src/migration/config-to-quon.md).

### Added (`quon explain copies`)

- **`quon explain copies`** lists the crates that building every run
  entry compiles more than once for one target, the entries that use
  each copy, and the feature difference behind it, traced down to the
  crate where it starts. It builds nothing.
- The `Finished` line of a multi-entry build says how many crates have
  more than one copy.

### Added (target flags)

- **`Target::rustc_flag(flag)`** passes a flag to every crate built for
  the target, dependencies and a built sysroot included, as Cargo's
  `[target.<triple>] rustflags` does. Build scripts see it in
  `CARGO_ENCODED_RUSTFLAGS`, and the cfg probe runs with it.
- **`Target::named(name)`** names a builtin triple apart, so one triple
  can be several targets, such as one per CPU, each with its own flags,
  features and artifacts.

### Added (workspace members in `quon.rs`)

- **`q.cargo_workspace(dir).members()`** returns the workspace's member
  packages with their path and `[dependencies]`, so `quon.rs` can
  declare an entry per package instead of listing them. Quon reads the
  manifests while the script runs, so a new package shows up on the
  next command without recompiling `quon.rs`.

### Added (several run entries)

- **`q.run` can be called once per binary.** Each entry takes a
  `.name(..)`, and `quon run <entry>`, `quon build <entry>`,
  `quon check <entry>`, `quon clippy <entry>`, `quon configure <entry>`
  and `quon test --entry <entry>` pick one. `RUN = "<name>"` in
  `.quon-config`, or `QUON_RUN`, sets the default.
- **Cargo features resolve per entry**, as `cargo build -p` resolves
  them, so two entries can enable features of one dependency that
  exclude each other, such as two chip features of a HAL.
- `quon build`, `check` and `clippy` with no name build every entry in
  one schedule. A crate two entries build the same way compiles once.
  `quon.rs` runs and the Cargo manifests are read once for all
  entries. `quon test` with no name runs only the host tests.
- `quon run` takes runner arguments only after `--`, so
  `quon run <entry>` names an entry.

### Changed (build speed)

- **Each input file is hashed about once per build.** Writing a
  record hashed every declared input again, including every
  dependency rlib and the sysroot. For a set of 13 STM32 firmware
  boards, hashing took 18.7s of CPU in a 26s cold build, and 0.45s
  per crate after a one-line edit. The store now reuses a hash while
  the file keeps the mtime and size it had when hashed. Rebuilding
  one board after an edit went from 1.5s to 0.37s.
- **File content hashes use BLAKE3 instead of SHA-256.** It hashes a
  50 MB rlib 4.7 times faster and is also a cryptographic hash.
  Existing records are in the old format, so the first build after
  upgrading rebuilds everything.

### Changed (prebuilt sysroots)

- **Builtin targets link the toolchain's prebuilt `core`, `alloc` and
  `compiler_builtins`**, as Cargo does, instead of building them from
  `rust-src`. The target must be installed with
  `rustup target add <triple>`, and Quon names that command when it is
  not. Building them from source took 11s for
  `thumbv7em-none-eabihf`, and no other crate of the target could start
  before it ended.
- **`Target::builtin(..).build_std()` builds them from source**, as
  every builtin target did before. Custom JSON targets always do.

### Added (size opt-levels)

- **`Profile::opt_level` takes `"s"` and `"z"`**, rustc's levels that
  optimize for size, as well as 0 to 3 and an `OptLevel`. Firmware
  commonly builds its release profile with `"s"` to fit in flash. A
  build script sees the level in `OPT_LEVEL`, as under Cargo.

### Added (debug assertions and overflow checks)

- **`Profile::debug_assertions(bool)` and
  `Profile::overflow_checks(bool)`** pass rustc's
  `-C debug-assertions` and `-C overflow-checks`. `Profile::dev()` turns
  both on and `Profile::release()` turns both off, as in Cargo, so
  `Profile::dev().opt_level(1)` keeps the checks that rustc would drop
  above opt-level 0. A profile that sets neither builds as before. A
  build script sees `CARGO_CFG_DEBUG_ASSERTIONS` when the profile turns
  debug assertions on.

### Added (host-test features)

- **`q.host_tests().package_with_features(name, [..])`** runs a
  package's host tests with those features. The package resolves on
  its own, as `cargo test -p <name> --features ..` does, so two
  packages can enable features of one dependency that exclude each
  other, such as two chip features of a HAL.

### Added (library)

- **The `quon` crate is a library.** `quon::run(dir, args)` runs the
  command line in-process, and `quon::Project` lists the run entries
  and where each one's binary is written. A repository's `xtask` can
  depend on Quon through its Cargo lock instead of an installed binary.
  See [`docs/src/tooling/library.md`](docs/src/tooling/library.md).
- **`q.run(..).meta(key, value)`** attaches strings to a run entry,
  which `quon::Entry::meta` returns. Quon does not read them, so a tool
  can keep per-entry data such as a chip name in `quon.rs`.
- `quon run` no longer swallows SIGINT and SIGTERM after its runner
  exits; they take their default action again.

### Fixed (Cargo workspaces)

- **A git dependency, or one from a registry other than crates.io, no
  longer fails every build.** Quon reports it only when a build uses it,
  so a workspace member that only Cargo builds, such as an `xtask`, can
  depend on one.

### Fixed (path dependencies)

- **A path dependency in another workspace can use `workspace = true`.**
  It inherits `[workspace.package]` fields, `[workspace.dependencies]`
  entries and `[workspace.lints]` from its own workspace root, as under
  Cargo. Before, every such field was an error.
- **The `[lints]` of path dependencies outside the workspace apply.**
  Cargo passes them to rustc; Quon dropped them, so a crate with
  `unsafe_code = "forbid"` could build under Quon and fail under Cargo.

### Fixed (images)

- **Quon no longer depends on GPL-3.0 code.** hadris-fat 1.x, the FAT
  writer behind `image()`, pulled in noalloc, which is GPL-3.0. Quon
  now uses hadris-fat 2.5, which does not.
- **Upgrading hadris-fat rebuilds every image.** The image cache key
  was meant to hold the hadris-fat version but always held "unknown",
  so an image written by an older FAT writer could be reused.

### Fixed (fmt)

- **`quon fmt` formats only declared crates and workspace members.**
  It rewrote the sources of registry and out-of-tree path
  dependencies too.

### Fixed (host tests)

- **`test = false` on a `[lib]`, `[[bin]]` or `[[test]]` target leaves
  it out of `quon test`**, as under `cargo test`. Before, a firmware bin
  with `test = false` was built for the host and failed to compile.
- A host-test lib or bin with `harness = false` is an error. Quon
  built it with libtest anyway.

### Added (clippy arguments)

- **`quon clippy -- <args>`** passes `<args>` to clippy for every
  linted crate, as `cargo clippy -- -D warnings` does. A proc macro's
  lint pass now also gets the manifest's `[lints.clippy]` levels.

### Added (compile-time environment)

- **`q.env(key, value)` sets a variable for every rustc run and build
  script**, as Cargo's `[env]` does, for variables crates read while
  they compile, such as `DEFMT_LOG`. The value is part of every crate's
  command, so changing it rebuilds every crate.

### Changed (renamed to Quon)

- **Gluon is now Quon.** The `gluon` name on crates.io belongs to the
  Gluon scripting language, so the project is renamed before its first
  release there. The CLI package and binary are `quon`, installed with
  `cargo install quon`, and the libraries are `quon-core`,
  `quon-config`, `quon-model`, `quon-runner` and `quon-runner-qemu`.
- `gluon.rs`, `gluon_config`, `.gluon-config`, `gluon.lock`, `GLUON_*`,
  `~/.gluon` and the `gluon-<name>` plugin prefix are renamed to their
  `quon` forms.

### Added (runner plugins)

- **Runner plugins.** A runner can be served by a plugin:
  `cfg.runner("board").plugin("probe-rs").setting("chip", "...")`.
  Quon looks the plugin up among the runners built into `quon`, then
  as `quon-runner-<name>` on `$PATH`, and calls it over a JSON
  protocol on stdin and stdout: `validate` before the build, `prepare`
  after it. The new `quon-runner` crate has the protocol types, the
  `Runner` trait and `serve()` for a plugin's `main`.
- **QEMU is a runner plugin.** The QEMU argv, boot mode, OVMF and disk
  code moved from `quon-core` to the new `quon-runner-qemu` crate,
  which `quon` builds in as the `qemu` plugin and which also builds a
  `quon-runner-qemu` executable. `quon.rs` is unchanged: the QEMU
  builder's values become the plugin's settings, and the profile's
  `qemu_*` values its overrides. The commands are the same as before.
  `--uefi`, `--direct` and `--bios` reach a plugin as a `boot_mode`
  override, and `--gdb` as a request field; the GDB note printed
  before the run now comes from the runner.
- A runner of another kind refuses `--gdb` and the boot mode flags
  with "needs a QEMU runner or another runner plugin".

### Added (stable Rust and release profiles)

- **Stable toolchains.** Quon no longer passes `-Zbinary-dep-depinfo`.
  It declares every crate file rustc may load as a freshness input
  itself: the `--extern` files, the outputs of every unit and prebuilt
  artifact reachable through them, and a custom sysroot's rlibs. Host
  builds and built-in targets now build on stable Rust. Custom `.json`
  targets still need nightly. The first build after upgrading rebuilds
  everything, because every rustc command changed.
- **`-r` / `--release`** selects the `release` profile, as in Cargo.
- **`quon init` writes a release profile.** It inherits from `dev` and
  sets `opt_level(3)` and `debug_info(false)`. `dev` is now the
  explicit `default_profile`.

### Added (host tests and boot tests)

- **Host tests.** `cfg.host_tests().package("name")` makes `quon test`
  build the package's lib and its `harness = true` tests with
  `rustc --test` for the host, with the package's dev-dependencies, and
  run them from the package directory. They resolve features in their
  own domain, so neither the kernel's features nor those of build
  scripts and proc macros reach them. `quon build` builds none of
  them. Proc-macro packages are not supported yet.
- **Bin unit tests.** A host-test package's bins are built with
  `--test` too, as `<bin>@bin-test`. When the package has tests in
  `tests/`, its bins build and those tests get `CARGO_BIN_EXE_<name>`
  and `CARGO_TARGET_TMPDIR` (`build/tmp`), as under Cargo.
- **`cfg.host_tests().env(name, value)` and `.env_path(name, path)`**
  set variables for every host test run; `.env_path` resolves against
  the project root.
- **Quon builds and tests itself.** The repository's `quon.rs` uses a
  host profile; `quon test` runs the same 860 tests as
  `cargo nextest run --workspace`.
- **Doctests.** Host-test packages also run their doctests with
  `rustdoc --test`, linking the lib and its dev-dependencies, with the
  lib's features and build-script output. `doctest = false` turns them
  off. They are reported as `host test <package> (doctests)`.
- **Boot tests.** `cfg.boot_test("name").profile("test")` makes
  `quon test` build that profile and boot its run binary, passing when
  QEMU exits with the success status. This runs a kernel's own test
  runner, such as one a config option turns on.
- **`CrateType::Test`**, a libtest executable built with `--test`.

### Added (Cargo projects)

- **A package without `[workspace]` loads as a workspace of one**, as
  under Cargo. A package inside another workspace's directory is an
  error that names the workspace root, unless that workspace excludes
  it.
- **`quon init` writes a host build** when no target is named and no
  member looks like a kernel (no `*.ld` beside it, no `#![no_main]` or
  `#![no_std]` bin): a `.target_host()` profile, one host group and
  `host_tests()` for every member.

### Fixed

- **`no_std` binaries link without a global allocator.** Quon passed
  `alloc` to every cross crate with `--extern`, so a dependency that
  named it, even in an intra-doc link, depended on it, and the binary
  failed with "no global memory allocator found". `alloc` now comes
  from the sysroot, so a crate that uses it needs
  `extern crate alloc;`, as with Cargo.
- **A rustup toolchain switch is picked up.** The cached rustc info
  was keyed on `$RUSTC` and the binary's mtime only, so adding a
  `rust-toolchain.toml` or changing the rustup default kept the old
  compiler. Quon now checks `rustc --print sysroot` against the cache
  unless `$RUSTC` is an absolute path.

### Added (host profiles)

- **`profile.target_host()`** makes a profile build for the host. It
  needs no declared target and builds no sysroot or config crate. With
  no runner named, `quon run` runs the binary directly, passing the
  arguments after `--`. `ResolvedProfile::target` is now a
  `TargetSpec`; `cross_target()` gives the handle of a cross profile.

### Changed (runners)

- **Runners replace `cfg.qemu(...)`.** `cfg.runner(name)` declares how
  a profile's run binary is started, and `profile.runner(name)` picks
  one, inherited through `inherits`. `quon run`, the integration tests
  and the boot tests all go through the profile's runner. Kinds:
  - `.qemu(program)`: the old `cfg.qemu(...)` builder, unchanged. A
    project can now declare several.
  - `.command(&[...])`: runs the argv with each `{binary}` replaced by
    the binary's path, with `quon run -- <args>` appended, and passes
    on status 0 or `.success_exit_code(n)`.

  Migrate with `cfg.runner("qemu").qemu("qemu-system-x86_64")...` and
  `.runner("qemu")` on each root profile. `--gdb` and the boot mode
  flags fail on a command runner.
- `Error::QemuNotConfigured` is now `Error::NoRunner { profile }`,
  `QemuBinaryNotFound` is `RunnerProgramNotFound`, and `QemuTimeout` is
  `RunTimeout`. `run::QemuInvocation` is `run::Invocation`, which
  carries the runner's name and success status; `run_test` takes only
  the invocation, and `TestOptions::qemu_args` is `runner_args`.
- Failure lines name the runner: `FAILED (runner 'qemu' exited with 1)`.
- **`profile.boot_binary(...)` is now `profile.run_binary(...)`.** A
  profile's binary need not boot: a host tool's is run directly.
  `Error::NoBootBinary` is now `Error::NoRunBinary`.

### Changed (`quon test`)

- **`quon test` runs host tests, then integration tests, then boot
  tests**, and prints one summary. A failed run no longer hides the
  results of the others. `--host`, `--integration` and `--boot` select
  kinds. The filter goes to host tests as libtest's filter. The result
  lines read `integration test <name> ... ok`.
- **A project without `qemu(...)` can run `quon test`** when it has
  only host tests.
- `quon_core::run::prepare_tests` no longer builds; call
  `run::build_tests` first and pass its summary. `run_test` takes the
  model and a `QemuInvocation`.

### Changed (quon.rs entry point and `~/.quon`)

- **`quon.rs` now defines `pub fn config(cfg: &mut Config) -> Result<()>`
  instead of `main`.** Quon generates the `main` that creates the
  `Config`, calls `config` and emits the model. To migrate, replace
  `fn main() -> Result<()> { let mut cfg = Config::new(); ... cfg.emit() }`
  with `pub fn config(cfg: &mut Config) -> Result<()> { ... Ok(()) }` and
  pass `cfg` instead of `&mut cfg` to helpers. A script that still
  defines `main` fails to compile with a note saying this.
- **The quon-config library that `quon.rs` links against is built once
  per Quon version and rustc into `~/.quon`** (or `$QUON_HOME`) and
  shared by every project, instead of into each project's `build/`.
  `quon` embeds the quon-config and quon-model sources and its
  `Cargo.lock`, and builds the library from them offline first, so it
  no longer needs the checkout it was installed from.
  `QUON_CONFIG_SOURCE_DIR` is gone. Concurrent `quon` processes build
  an entry once.
- **`quon configure` declares `quon.rs` as a library crate** in
  `rust-project.json`, and points rust-analyzer at the quon-config
  sources the library was built from.

### Added (`quon setup`)

- **`quon setup`** builds the quon-config library for the current
  rustc ahead of time and prints its directory. Other commands build
  it on first use.

### Added (Dev-dependencies for integration tests)

- **`harness = false` tests can use `[dev-dependencies]`.** When a root
  package has tests, Quon runs a second feature resolve for them, as
  `cargo test` does: the package's dev-dependencies apply (with
  `[target.'cfg(...)'.dev-dependencies]` keys matched against the
  group's target), and their features unify with the normal ones. The
  kernel's resolve still ignores dev-dependencies, so the kernel's
  artifacts do not change when tests are added. A dependency whose
  features a dev-dependency grows builds a second time for the tests,
  under a `@test-pass` name. `quon build` and `quon run` leave the
  dev-dependencies and those copies out. See
  `docs/src/running/integration-tests.md`.

### Added (Modular workspace config)

- **`quon.rs` config can now be split across multiple files** using
  Rust's `mod` + `#[path]` attribute. Each subsystem exposes
  `pub fn register(cfg: &mut Config) -> Result<()>` and is wired in
  from the root script. Cross-subsystem references to shared targets
  work by name — `cfg.target("x86_64-unknown-none")` from any
  subsystem returns the same handle as the root's declaration, so
  there's no need to thread `Handle<TargetDef>` through `register()`
  signatures. See
  `docs/src/configuration/quon-rs.md#splitting-config-across-files`.
- **Script-runner cache invalidates on transitively-included files.**
  The cache layer now hashes the rustc-emitted depfile's full source
  list — every file the script touched via `mod` / `#[path]` — so
  editing a subsystem `quon.rs` triggers the same recompile as
  editing the root entrypoint. Previously the cache key hashed only
  the entrypoint and silently served stale binaries on multi-file
  edits.
- **`Config::target` / `Config::target_spec` are now idempotent on
  identical re-declarations.** Calling `cfg.target("x86_64-unknown-none")`
  multiple times (e.g. once per subsystem) is silent and returns the
  same handle. A `"target ... is redefined"` diagnostic still fires
  when a second call would *change* the existing entry — different
  spec path, or a switch between builtin and custom-spec.

### Added (RawBinary debug ergonomics)

- **`RawBinary` crates now emit `<name>.elf` alongside `<name>.bin`.**
  rustc + lld produce a normal ELF; a post-link `rust-objcopy -O binary`
  step converts it to the flat `<name>.bin` (still the build-model
  primary artifact). The `.elf` preserves full debug info so `gdb`,
  `addr2line`, `objdump`, and `readelf` work against BIOS-stage and
  other RawBinary artifacts. Requires `rust-objcopy` from the
  `llvm-tools-preview` rustup component (`rustup component add
  llvm-tools-preview`). A linker script that contains
  `OUTPUT_FORMAT(binary)` is now incorrect — quon controls output
  format directly via the rustc invocation, and the build fails with a
  diagnostic pointing at the offending directive. See
  `docs/superpowers/specs/2026-05-05-rawbinary-elf-emission-design.md`.

### Added (Cross-target vendored builds)

- **`ExternalDepDef::target` selects which target a vendored dep is
  built for.** `cfg.dependency("bitflags").version("2.11").target(kernel)`
  builds bitflags against the user's custom target with the quon-built
  sysroot via the quon-driven rustc pipeline, instead of going through
  the host cargo-as-builder path. `.target_host()` is also available;
  unannotated deps stay on the host pipeline. See
  `docs/superpowers/specs/2026-05-04-cross-target-vendor-builds-design.md`
  for the full design and slice-level commit log.
- **Per-target `cargo vendor` scratch workspaces** under
  `build/vendor-workspaces/<target>/` (host pipeline moved from a flat
  `build/vendor-workspace/` to `build/vendor-workspaces/host/` for
  symmetry). Each cross target gets its own synthesised `Cargo.toml`
  listing only the deps tagged for that target; `cargo vendor`
  populates `vendor/` for that workspace and `cargo metadata --frozen`
  lowers the resolved graph into `VendorCrateBuild` scheduler
  descriptors.
- **`DagNode::VendorCrate` scheduler variant** dispatches each
  vendored crate through `compile::vendor_command` against the
  per-target sysroot, with output rlibs landing under
  `build/cross/<target>/<profile>/vendor-deps/<name>/`. Proc-macro
  deps re-target to the host as expected.
- **`quon.lock` schema v2** adds per-package `target` and `kind`
  fields. Loading a v1 lockfile is accepted (`target` defaults to
  `"host"`, `kind` defaults to `"lib"`) so existing on-disk
  lockfiles migrate transparently on the next `quon vendor`.
- **Cache invalidation propagates dep + sysroot changes.** A vendor
  crate's freshness query folds in (a) every dep's output rlib path
  and (b) the per-target sysroot lib dir's `*.rlib` set, so a sysroot
  rebuild or upstream-dep rebuild invalidates the consumer's cache
  via the existing two-tier mtime+sha256 check.
- **Vendor fingerprint folds in target.** Adding or removing
  `.target("kernel")` to an existing dep changes the fingerprint and
  forces a re-vendor. The fingerprint hashes the *target name* (not
  the arena handle index) so it stays stable across runs that
  reorder Rhai-driven inserts.

Behaviour for host-only projects is unchanged except for the
`build/vendor-workspace/` → `build/vendor-workspaces/host/` rename;
existing `quon.lock` files re-fingerprint and re-vendor once on
upgrade, then settle.

### Added (Stage 9 — architecture docs + comment cleanup)

- **Four new architecture docs** consolidate scattered
  architectural-invariant comments under `docs/src/architecture/`,
  linked from a new "Architecture" section in `SUMMARY.md`:
  - `cache.md` — two-entry-kind manifest, freshness algorithm,
    two-tier source check, eager fingerprint population, atomic
    write-temp + fsync + rename, schema versioning, composite
    `aux_hash` domain separation, canonical rustc-flag token order.
  - `determinism.md` — `BTreeMap`/`BTreeSet` everywhere, edge
    dedup invariant in `scheduler::dag` (with the deadlock incident
    that motivated it), cache-key stability, multi-thread vs `-j1`
    semantics.
  - `concurrency.md` — multi-process advisory `flock` on a sibling
    lockfile, atomic manifest writes, the Stage 8 in-process snapshot
    dispatch protocol.
  - `external-deps.md` — rustc dep-info Make subset, QEMU CLI
    shape, OVMF system-path probe table, hadris-fat / FAT32
    assumptions, `RUSTC_BOOTSTRAP` rationale.

### Changed (Stage 9 — architecture docs + comment cleanup)

- **Module-level rationale comments shrink to pointers.** Long
  module-doc blocks at `cache/{mod,manifest,depinfo,lock}.rs`,
  `compile/{compile_crate,extern_map}.rs`, `scheduler/{mod,dag,snapshot}.rs`,
  `image/mod.rs`, `run/ovmf.rs`, and `sysroot/mod.rs` now point at
  the architecture docs instead of duplicating their content. The
  `scheduler::dag` `BTreeSet`-not-`Vec` warning stays inline at the
  violation site (the `edges_out` type declaration), as does the
  volume-label collision rationale in `image/fat.rs` and
  per-function operational comments throughout.
- **`scheduler/mod.rs` module doc no longer mentions the deleted
  `helpers` submodule.** Stage 6 removed `helpers/`; the module-doc
  references are updated to describe the per-variant dispatch
  helpers and link to the concurrency architecture doc.
- **No behaviour changes.** Pure documentation work. `cargo build`,
  `cargo clippy`, and `mdbook build` are all clean; pre-existing
  rustdoc warnings unrelated to this stage are unchanged.

### Changed (Stage 8 — scheduler `ArtifactsSnapshot` abstraction)

- **Per-node dispatch arms no longer see shared mutable state.**
  Each `DagNode` variant runs through a free function
  (`dispatch_sysroot`, `dispatch_config_crate`, `dispatch_crate`,
  `dispatch_esp`, `dispatch_image`, `dispatch_rule`,
  `dispatch_build_step`) that takes an immutable
  `&ArtifactsSnapshot` and returns a `NodeOutput`. The dispatcher
  arm acquires the state lock at most twice — once before the helper
  to clone the snapshot, once after to merge the helper's output —
  and per-node helpers themselves never touch a `Mutex`.
- **The four output mutexes collapse into one.** The previous
  `(Mutex<sysroots>, Mutex<artifacts>, Mutex<esps>, Mutex<images>)`
  field set on `PipelineDispatcher` is replaced by a single
  `Mutex<ArtifactsSnapshot>` bundling all four maps. `lock_or_poison`
  hits in `scheduler/mod.rs` are now confined to snapshot
  read-clone and merge sites — verified by `grep` per the Stage 8
  acceptance criterion.
- **Determinism guarantee at `-j1`.** A new
  `execute_pipeline_j1_is_deterministic_across_runs` test (`#[ignore]`-
  gated on `rust-src` like the existing e2e tests) asserts two
  consecutive `-j1` builds over the same fixture produce
  byte-identical artifact paths and binary contents.
- **No behaviour or on-disk format changes.** The refactor preserves
  every prior semantic — including the deliberate "snapshot before
  helper, drop lock, merge after" pattern that keeps parallel
  compilations from serialising on a single mutex.

### Changed (Stage 7 — post-compile layering)

- **`PartitionFormatter` trait isolates per-partition filesystem
  formatting.** A new `image::formatter::PartitionFormatter` trait
  (one method: `format_in_place`) plus a `select_formatter(fs_type,
  ...)` dispatch function replace the inline
  `match rp.def.fs_type.as_str()` inside
  `image::format_partition_in_place`. `Fat32Formatter` lives next
  to the existing FAT32 formatting code in `image/fat.rs`. Adding
  a new partition filesystem now means implementing the trait and
  adding one arm to `select_formatter` — no edits to the
  dispatcher or `build_partitioned_disk`. Behaviour is bit-identical
  to Stage 6 for FAT32; the unknown-`fs_type` error message is
  preserved verbatim.
- **`BootModeAssembler` trait isolates per-mode QEMU argv wiring.**
  A new `run::boot_mode::BootModeAssembler` trait + three
  unit-struct impls (`DirectAssembler`, `UefiAssembler`,
  `BiosAssembler`) + `assembler_for(mode)` dispatch replace the
  100-LOC inline `match mode { ... }` in
  `run::qemu_cmd::build_qemu_command`. The coordinator handles only
  mode-agnostic concerns (memory, cores, display, serial, timeout,
  test-mode exit, user extras, plus the upstream UEFI-without-OVMF
  guard); per-mode argv lives in the assemblers. `qemu_cmd.rs`
  shrinks from 991 → 569 LOC. The `push_pflash` helper moved with
  `UefiAssembler`. Per-mode tests (Direct argv, UEFI pflash + ESP
  variants, BIOS disk + IDE wiring) moved into `boot_mode.rs`'s
  test module; coordinator-level tests (display, serial, timeout,
  test-mode, user-extras, UEFI-OVMF guard) stayed in `qemu_cmd.rs`.
- **`Substituter` value type collapses 3-arg substitution passing.**
  A new `rule::substitution::Substituter<'a>` (a value type, not a
  trait — there is one substitution implementation by design) holds
  the borrowed `(&RuleCtx, &RuleDef)` pair once and exposes five
  methods (`expand_args`, `expand_one`, `resolve_var`,
  `resolve_image`, `resolve_artifact`). The five freestanding
  functions in `rule/mod.rs` (`substitute_args`, `substitute_one`,
  `resolve_var`, `resolve_image`, `resolve_artifact`) and their
  3-arg `(ctx, rule, ...)` plumbing are gone. `rule/mod.rs` shrinks
  from ~1390 → 419 LOC (substitution body + tests + helpers
  relocated to `rule/substitution.rs`). The internal call sites in
  `RuleRegistry::dispatch` now construct one `Substituter` per rule
  invocation and reuse it for both `inputs` and `outputs`
  expansion.
- **No on-disk format / cache key changes.** `PartitionFormatter`
  and `BootModeAssembler` are pure call-site refactors; the FAT32
  on-disk layout, the QEMU argv shape per mode, and the rule
  substitution outputs are all bit-identical to pre-Stage-7. No
  cache invalidation is triggered by this stage.

### Changed (Stage 6 — compile pipeline tightening)

- **`CrateArtifactShape` is the single source of truth for crate
  path arithmetic.** A new `compile::CrateArtifactShape` struct holds
  `(out_dir, output_path, depfile_path)` for any crate; both
  `compile::command_builder::build_rustc_command` (write side) and
  `compile::artifact_path::expected_crate_artifact_path` (read side
  — analyzer-time) now consume it. The duplicated host/cross
  dispatch (rlib / dylib / bin / `.bin` / depfile) is gone.
- **`compile::compile_crate::compile()` decomposed.** The 160-LOC
  body shrinks to ~68 LOC by extracting `build_cache_key` (cache-key
  construction) and `prepare_compile_context` (everything before
  the rustc spawn). The third copy of the host/cross dispatch
  (previously inlined to compute `out_dir` for `mkdir -p`) is gone.
- **`BuildLayout::cache_key_suffix()`.** New method centralising
  the `""` / `":check"` / `":clippy"` suffix attached to per-crate
  cache keys; the inline `match ctx.driver()` in `compile_crate.rs`
  is replaced by a single call. No on-disk cache invalidation —
  key shapes are byte-identical to Stage 5.
- **`scheduler/helpers/` deleted.** The three per-node helpers were
  promoted to their natural homes:
  - `helpers::esp` → `generated::esp` (new top-level `generated/`
    module for "produced from the model" outputs).
  - `helpers::config_crate` → `generated::config_crate`.
  - `helpers::sysroot::ensure_sysroot_for_node` folded into
    `crate::sysroot` (32-LOC wrapper next to its delegate).

### Changed (Stage 5 — unified build-time freshness)

- **Unified build-time freshness via the extended `Cache`.** Sysroot,
  image, and ESP freshness records moved from sidecar stamp files
  (`<sysroot>/stamp`, `<image>.stamp.json`,
  `<esp_dir>/.esp-stamp.json`) into the cache manifest as
  `Composite` entries. The sidecar files are no longer produced.
- **`clean --keep-sysroot` now retains the sysroot fast-path.** The
  cache manifest is filtered (rather than deleted) so the next build
  reports the sysroot as cached. Both rustc-side
  `sysroot:<target>:<crate>` per-crate keys and composite-side
  `sysroot:<target>` keys are preserved.
- **Vendor freshness intentionally not migrated.** `quon.lock`
  remains the source of truth for vendor consistency — see
  `crates/quon-core/src/vendor/lockfile.rs` module docs for the
  rationale (cross-machine state, would force re-vendor on every
  fresh checkout).
- **Renamed `cache::CacheEntry` → `cache::RustcEntry`.** Internal
  rename to make the symmetry with the new `cache::CompositeEntry`
  explicit. No public-API consumers expected (quon-core's `pub`
  surface is internal-use-only).
- **`BuildLayout::sysroot_stamp` removed.** The sysroot stamp file
  no longer exists; the layout method that produced its path is gone.

### Cache impact (Stage 5)

- Cache manifest schema bumped 1 → 2. The first build after upgrading
  logs `cache schema upgraded; rebuilt clean` and rebuilds from
  scratch. Subsequent builds are fully cached as before.

### Changed (breaking — model wire format)

- `BuildModel`: cross-references are handle-only. Parallel
  `name: String + name_handle: Option<Handle<...>>` pairs are gone;
  consumers read handles directly.
- `BuildModel.config_options`: now an `Arena<ConfigOptionDef>`
  (was `BTreeMap<String, _>`). Wire format changes (object → array).
- `BuildModel.qemu`, `BuildModel.bootloader`: now `Option<...>`.
- New typed enums: `PanicStrategy`, `BootloaderKind` (with
  `Custom(String)` escape hatch), `ImageFormat`, `TargetSpec`,
  `DepResolution`.
- `quon run` against a `quon.rs` with no `cfg.qemu(...)` block
  now fails with a typed `Error::QemuNotConfigured` carrying a
  user-actionable message.

### Cache impact

- First build after this version is a full cache miss (manifest
  schema bumped). Subsequent builds resume normal caching.

### Removed

- `quon.rhai` configuration format. Every project must now declare
  its build via `quon.rs`. See
  [`docs/src/migration/rhai-to-rust.md`](docs/src/migration/rhai-to-rust.md)
  for the surface mapping.
- `quon-lsp` crate. rust-analyzer covers the editor story for
  `quon.rs`.
- `tree-sitter-rhai` crate.
- `quon-core::engine` module and the `quon_core::evaluate` public
  function. `quon_core::load_build_model` is the surviving entry
  point.
- `quon internal dump-dsl` hidden subcommand.
- Script-backed rule handlers (`RuleHandler::Script`). Built-in
  handlers (`copy`, `exec`) are unchanged.
- `pipeline(...)` Rhai builder. The DAG generated from
  `artifact_deps` and `artifact_env` is the surviving ordering
  primitive.
- `rhai` workspace dependency.

## [0.1.0] - 2026-04-09

### Added

- **Rhai-scripted build configuration** — declare projects, targets,
  profiles, groups, and crates in `gluon.rhai`
- **Direct `rustc` invocation** — bypass Cargo, assemble per-crate
  flags, and compile custom sysroots (`core`, `alloc`,
  `compiler_builtins`) for arbitrary target triples
- **Build cache** — hybrid mtime + SHA-256 freshness checking with
  depfile tracking and file-level locking
- **DAG scheduler** — parallel worker pool with dependency-ordered
  compilation
- **Pipelines and rules** — user-definable build rules with an `exec`
  builtin for custom build steps
- **Config resolution** — interpolation, per-target overrides, and
  profile-based filtering
- **Kconfig-style typed configuration** — `.kconfig` file loader with
  bool/int/string/hex options, defaults, dependencies, and `select`
- **`gluon build`** — full compilation pipeline from config evaluation
  through sysroot and crate compilation
- **`gluon run`** — QEMU boot for direct and UEFI targets with
  `--no-build`, `--gdb`, signal forwarding, per-target QEMU args, and
  test mode
- **`gluon vendor`** — vendor external crate dependencies with lockfile
  tracking, checksums, and fingerprint-based staleness detection
- **`gluon check` / `clippy` / `fmt`** — forward to `rustc`/`clippy`/
  `rustfmt` with correct sysroot and flag assembly
- **UEFI bootloader+kernel support** — `artifact_env` for cross-crate
  artifact paths, ESP directory assembly, multi-target builds
- **Per-profile filtering** — select which crates and groups apply to
  each profile
- **`depends_on` expressions** — declare inter-crate build dependencies
  in Rhai config
- **CI pipeline** — GitHub Actions with fmt, clippy, test, vendor e2e,
  and dogfood (`gluon fmt --check`) jobs
- **mdbook documentation** — 26 pages covering getting started,
  configuration, build system internals, running/debugging, CLI
  reference, tooling integration, and cookbook recipes
- **Example `gluon.rhai` scripts** — basic kernel, UEFI bootloader, and
  Kconfig-style configuration examples
- **rust-analyzer integration** — `rust-project.json` generation for
  IDE support in bare-metal projects

[unreleased]: https://github.com/hxyulin/gluon/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/hxyulin/gluon/releases/tag/v0.1.0
