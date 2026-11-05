# Build Cache

Gluon uses a hybrid two-tier freshness model to avoid unnecessary
recompilation. The goal is to never recompile a crate whose inputs have
not changed, while also never silently using a stale artifact.

## How it works

### Fast path: mtime + size

For each input file, Gluon compares the last-modified time and file size
against the values in the node's fingerprint record. If both match, the file
is considered fresh. This check is nearly free -- it requires only a
`stat()` call per file.

### Slow path: SHA-256

When the mtime differs but the file might not have actually changed, Gluon
falls back to comparing SHA-256 content hashes. This happens in situations
like:

- `git checkout` or `git rebase` that updates mtimes without changing
  content
- CI cache restores that do not preserve timestamps
- Editors that write-then-rename (updating mtime even for no-op saves)

If the hashes match, the mtime was a false positive. Gluon refreshes the
cached mtime to avoid repeating the hash computation on the next build,
and skips recompilation.

## What triggers recompilation

A crate is recompiled when **any** of the following change:

- **Source files** -- content change detected via the two-tier model
  described above. The file list comes from rustc's depfile.
- **The rustc command** -- the assembled flags, environment and program
  are hashed. A new `--cfg` flag or a different optimization level
  recompiles the crate even if no source changed.
- **The toolchain** -- a different rustc binary or `rustc -vV` output.
- **Dependencies** -- the files passed to rustc for direct dependencies
  are inputs. A dependency rebuilt with different content recompiles
  its dependents; one rebuilt to identical bytes does not.
- **Environment variables** the crate reads with `env!` or
  `option_env!`.

An input modified while its compile was running also reruns the
compile on the next build, because the compile may have read the old
content.

`gluon -v build` prints why each node reran, for example
`rebuilding crate kernel (x86_64-unknown-none, dev): input
crates/kernel/src/main.rs changed`.

Warnings are stored with the record and printed again when a later
build finds the crate fresh.

## Fingerprint records

Freshness records live in `build/.fingerprint/`, one JSON file per
node (one per entry for an ESP). A node writes its record as soon as it
finishes, so a failed build keeps the records of everything that did
compile. A record holds:

- Input paths with their mtime, size and SHA-256 hash
- The hash of the command and of the toolchain
- The environment variables the run read, with their values
- The output paths

A missing or unreadable record makes that node rebuild. A build
directory from an older Gluon (with a `build/cache-manifest.json`) is
cleared once and rebuilt; Gluon prints a note when it does.

See [cache architecture](../architecture/cache.md) for the exact rule.

## Build output layout

```
build/
  sysroot/<target>/           # Custom sysroot (shared across profiles)
  generated/<project>_config/ # Generated config crate sources
  cross/<target>/<profile>/
    deps/                     # Cross-compiled libraries and the config crate
    final/                    # Cross executables, under their plain names
    esp/                      # EFI partitions
    incremental/              # Incremental state, one directory per unit
  host/
    deps/                     # Proc macros and the host crates they use
    profile/<profile>/deps/   # Host tools and build-step runners
  tool/check/                 # Output from gluon check
  tool/clippy/                # Output from gluon clippy
  .fingerprint/               # Freshness records
```

Every library file name carries the unit's metadata hash, for example
`libkernel_util-3f2a9c1d0b7e6a45.rlib`. Gluon passes the same hash to
rustc as `-C metadata`. The hash covers what makes a unit a distinct
artifact: package name, version and source, crate kind, target,
features, the hashes of its dependencies, and for project crates the
generated config crate's contents. It leaves out sources, flags, the
profile and the toolchain. A change to those rebuilds the file in place.
A change to the identity writes a new file next to the old one, so
switching between two config presets rebuilds nothing after the first
build of each.

Key layout decisions:

- The **sysroot** is shared across profiles and drivers (build, check,
  clippy). Compiling it once per target triple is sufficient.
- **Cross-compiled artifacts** are separated by target and profile. A
  debug build and a release build never share artifacts.
- The **`tool/`** subdirectories ensure that `check` and `clippy` outputs
  do not collide with `build` artifacts. Running `gluon check` will not
  invalidate your last `gluon build`.
- **Host-compiled crates** live in a separate `host/` directory because
  they are compiled for the host triple, not the target triple. Procedural
  macros, vendored host crates and the crates they depend on use a fixed
  host profile (no optimisation, no debug info) and `host/deps/`, so
  switching profiles does not rebuild them. `gluon check` and `gluon
  clippy` build them with plain rustc and share them with `gluon build`,
  because a proc macro needs real rlibs of its dependencies. `gluon
  clippy` then runs clippy-driver over your own proc macros and their
  host crates a second time, metadata only, into `tool/clippy/host/lint/`,
  so their lints are still reported. Other host
  crates (host tools, build-step runners) use the active profile and
  `host/profile/<profile>/deps/`, so each profile keeps its own copy.
- Old files are not deleted when a hash changes. They are never loaded,
  so they cost disk space only; `gluon clean` removes them.
