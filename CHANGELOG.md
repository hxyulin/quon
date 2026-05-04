# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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
