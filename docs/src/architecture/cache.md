# Build Cache

Every node that can skip its work (sysroot, sysroot crate, config
crate, build unit, build-script run, ESP entry, image, build step) proves it is fresh
with one function, `FingerprintStore::check`, and records a finished
run with one function, `FingerprintStore::record`. Both live in
`cache::store`. Rules are not cached: they run arbitrary commands with
no declared outputs, so any cache would be a guess.

The rule that governs both sides is: **when in doubt, invalidate**. A
cache that serves a stale output is worse than one that is slow.

## The query

```rust
pub struct FreshnessQuery<'a> {
    pub key: &'a str,
    pub command: [u8; 32],
    pub toolchain: [u8; 32],
    pub declared: &'a [PathBuf],
    pub outputs: &'a [PathBuf],
    pub command_env: &'a BTreeMap<OsString, OsString>,
}
```

- **`key`** names the record. A node may own several (an ESP owns one
  per entry). Unit keys contain the unit's metadata hash, and keys of
  units whose outputs differ per driver end in `:check` or `:clippy`.
- **`command`** is, for process nodes, `hash_argv` over program, argv,
  env and cwd. Other nodes hash their definition with `hash_parts`:
  the sysroot hashes the target spec, panic strategy and rust-src path;
  an ESP entry its ESP name, destination and source kind; an image its
  serialised `ImageDef`, the `hadris-fat` version and its entries'
  source kinds.
- **`toolchain`** is `RustcInfo::toolchain_hash`: the rustc path plus
  `rustc -vV`'s version, commit and host. Every node passes it. It is
  not part of any metadata hash, so a toolchain change rebuilds in
  place under the same file names.
- **`declared`** are inputs known before the run: direct deps' outputs
  (the files passed with `--extern`, including the config crate and
  cargo-built prebuilt artifacts), `artifact_env` artifacts, the linker
  script and custom target spec, the generated config-crate source, the
  rustc binary for the sysroot, an image's entry files, a build step's
  runner and inputs.
- **`outputs`** must all exist for a hit, so deleting any of them forces
  a rerun.
- **`command_env`** is the environment the node sets on its child.

Discovered inputs are not in the query. They come from the record: the
previous run's depfile (sources plus, with `-Zbinary-dep-depinfo`, every
rlib, rmeta and proc-macro dylib rustc loaded) and its `# env-dep:`
lines. A fresh check therefore reads one record file and stats the
inputs; it does not parse a depfile.

## The rule

A record is fresh if and only if, in order:

1. It exists and its `format` is current.
2. `command` and `toolchain` equal the record's.
3. Every output exists.
4. The declared input set equals the recorded declared set. Adding or
   removing a dep or an image entry is a change even if every remaining
   file is unchanged.
5. Every declared and discovered input exists, carries no dirty mark,
   and either its mtime and size equal the recorded stamp or its
   SHA-256 equals the recorded hash. A hash match rewrites the stamp so
   the next check takes the mtime path.
6. Every recorded environment variable has the value the next run would
   see: the node's own command env if Gluon sets it there (such as
   `CARGO_PKG_NAME` or `KERNEL_PATH`), else Gluon's environment. Unset
   and empty are different values.

Otherwise the node is stale, with the first failing step as the reason
(`StaleReason`). `gluon -v build` prints it for each rerun node:

```
rebuilding crate kernel (x86_64-unknown-none, dev): input /src/kernel/src/main.rs changed
```

Directory mtimes are not tracked. They change whenever an editor
creates and deletes a swap file. A new module file needs no directory
check: the `mod foo;` line that pulls it in edits a file already in the
record, and the next depfile lists the new file. A deleted file fails
its stat.

### Two tiers per file

- **mtime and size** match: fresh, one `stat` and no read.
- They differ: hash the file. A match means the mtime moved without a
  content change (`touch`, a checkout, a CI cache restore, an editor
  that saves without changes); the stamp is refreshed in place. A
  mismatch is a change.

Recording hashes every input eagerly. Lazy hashing would make the first
touch rebuild without storing a hash, so the second touch would rebuild
too.

### Early cutoff

A dependency that reruns and writes byte-identical output fails the
mtime check, passes the hash check, and its dependents stay fresh. No
dependency-fingerprint chain is needed. Rlibs rarely qualify, since
they embed spans; build-script output and generated files often do.

### Inputs modified during a run

`record` hashes the inputs after the tool exits, so it sees them as
they are then, not as the tool read them. The caller passes
`started_at`, taken just before the spawn. An input whose mtime is at or
after it may have been edited while the tool ran, so its stamp is
marked dirty, and a dirty stamp never matches: the next check reports
`input ... was modified during the last build`. Cargo uses the same
rule. A filesystem with coarse mtimes can round an edit made during the
run down to before `started_at`, which this does not catch. A file with
an mtime in the future always reruns its node.

### Immutable sources

Files under Cargo's registry source cache (`$CARGO_HOME/registry/src`)
are left out of records, so a no-op build does not stat them. Cargo
extracts a package there once and never changes it, and the package's
name, version and source are in the unit's metadata hash, so in its
key and command. Vendor directories are checked like any other source,
because people patch vendored crates.

## Records on disk

Each record is one JSON file in `build/.fingerprint/`, shared by every
driver:

```
build/.fingerprint/
    crate_cross_kernel_dev_x86_64-unknown-none-3f2a9c1d.json
    crate_host_minimal_derive_b4c17d04da30ca6e-9a1b2c3d.json
    crate_host_minimal_derive_b4c17d04da30ca6e-9a1b2c3d.stderr
    sysroot_x86_64-unknown-none-0c4e1a2b.json
    esp_boot_EFI_BOOT_BOOTX64.EFI-77d0e3f1.json
```

The file name is the key with characters outside `[A-Za-z0-9._-]`
replaced by `_`, cut to 80 bytes, plus the first 8 hex digits of the
key's SHA-256 when anything was replaced or cut, so two keys never
share a file. A record holds the format, key, command, toolchain, the
declared and discovered stamps (mtime, size, SHA-256, dirty), the env
values, the outputs, `started_at`, and for build-script runs the parsed
output (`data`). Hashes are hex strings so the
files can be read when a cache misbehaves.

Why one file per record instead of one manifest:

- A failed or interrupted build keeps every record it wrote.
- No shared lock and no whole-file rewrite when one stamp refreshes.
  Each node owns its keys.
- `gluon clean --keep-sysroot` keeps `sysroot_*` files.
- A no-op build reads only the records of nodes in the current DAG.

Records are written to a temp file and renamed. There is no fsync:
after a crash a record is the old one, the new one, or unparsable, and
an unparsable record is stale.

### Failures

A node writes its record only after its outputs are complete: after
rustc, after any post step (objcopy for raw binaries) and after its
depfile parsed. A failure anywhere before leaves the old record, which
no longer matches, so the next build reruns the node. An unreadable or
unparsable record is stale (`FormatChanged`), never a build error.

### Build-script runs

A `RunBuildScript` node's command is the script binary's path, the
environment Gluon sets and the package directory. Its declared input is
the script binary, its discovered inputs are the `rerun-if-changed`
paths (every file in the package when the script prints none, skipping
`target/`, dot-directories and Gluon's build directory), and its
env values are the `rerun-if-env-changed` names. The record's `data`
holds the parsed output, so a hit hands it to the owner without running
the script. The output reaches the owner's argv and env, so a changed
output reruns the owner and a rerun with the same directives does not.
A `rerun-if-changed` path that does not exist leaves the run unrecorded,
so it reruns every build, as in Cargo.

### Replayed warnings

When a run prints to stderr, its record gets a `.stderr` sidecar. A
later cache hit prints the sidecar again, so warnings stay visible on
every build, as in Cargo. Vendored units are compiled with
`--cap-lints allow` and print none.

### Format changes and the old manifest

A record with another `format` is stale, per record. Gluon versions
before this format kept every record in `build/cache-manifest.json`.
On the first build that finds that file, Gluon deletes it together with
the trees it described (`host/`, `tool/`, and everything under
`cross/<target>/<profile>/` except `final/` and `esp/`), prints one
note, and builds clean. The sysroot stays on disk but has no record, so
it rebuilds too.

## Canonical rustc-flag token order

A unit's command hash is `hash_argv` over the assembled argv, so any
reordering of the flag set reruns every unit once. The order is fixed:

```
input → crate-name → crate-type → edition → sysroot? → target? →
out-dir → implicit-externs? → config-crate-extern? → emit →
explicit-externs → dependency-search-dirs → proc-macro-extern? →
opt-level → debug-info → lto? → feature-cfgs → cfg-flags →
cap-lints? → panic-strategy? → extra-rustc-flags → incremental? →
linker-script? → raw-binary-output? → metadata → extra-filename?
```

`?` marks tokens that appear only when applicable (host crates skip
`sysroot`/`target`, vendored crates skip `incremental` and add
`cap-lints`, etc.). Declared and vendored crates share this order: both
are build units assembled by `compile::command_builder`.

rustc does not list two files it reads in its depfile: the linker
script passed as `-C link-arg=-T<path>` and a custom target spec passed
to `--target`. `RustcCommandBuilder` records both, and
`compile_and_cache` declares them. The depfile parser only handles the
subset of Make syntax rustc emits; see
[external dependencies](./external-deps.md).
