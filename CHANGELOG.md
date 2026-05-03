# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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
- **Vendor freshness intentionally not migrated.** `gluon.lock`
  remains the source of truth for vendor consistency — see
  `crates/gluon-core/src/vendor/lockfile.rs` module docs for the
  rationale (cross-machine state, would force re-vendor on every
  fresh checkout).
- **Renamed `cache::CacheEntry` → `cache::RustcEntry`.** Internal
  rename to make the symmetry with the new `cache::CompositeEntry`
  explicit. No public-API consumers expected (gluon-core's `pub`
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
- `gluon run` against a `gluon.rs` with no `cfg.qemu(...)` block
  now fails with a typed `Error::QemuNotConfigured` carrying a
  user-actionable message.

### Cache impact

- First build after this version is a full cache miss (manifest
  schema bumped). Subsequent builds resume normal caching.

### Removed

- `gluon.rhai` configuration format. Every project must now declare
  its build via `gluon.rs`. See
  [`docs/src/migration/rhai-to-rust.md`](docs/src/migration/rhai-to-rust.md)
  for the surface mapping.
- `gluon-lsp` crate. rust-analyzer covers the editor story for
  `gluon.rs`.
- `tree-sitter-rhai` crate.
- `gluon-core::engine` module and the `gluon_core::evaluate` public
  function. `gluon_core::load_build_model` is the surviving entry
  point.
- `gluon internal dump-dsl` hidden subcommand.
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
