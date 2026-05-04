# Build Cache

The cache is the load-bearing correctness layer for gluon's
incrementality story. Every build artefact that can be skipped must
first be proved fresh by `Cache::is_fresh` (or
`Cache::is_composite_fresh`); every artefact that's actually (re)built
gets recorded via `Cache::mark_built` (or `mark_composite_built`).

The rule that governs both sides is simple: **when in doubt,
invalidate**. A cache that silently uses stale outputs is worse than
one that's slow.

## Two entry kinds

The on-disk manifest carries two parallel maps:

- **`rustc`** — per-crate compile cache. Keyed by a caller-chosen
  string (typically the crate id). Records the SHA-256 digest of the
  rustc invocation (`argv_hash`), the source set with per-file
  fingerprints, parent-directory mtimes, and the declared output path.
- **`composite`** — non-rustc artefacts (sysroot, ESP, image, vendor).
  No `argv_hash`; instead an opaque `aux_hash` rolls in non-file
  inputs (model definition bytes, tool version strings). File inputs
  use the same fingerprint shape as rustc entries.

Splitting them keeps each entry shape minimal — composite entries
have no meaningful argv, rustc entries have no aux state — and lets
`Cache` expose two type-safe APIs without runtime tag checks.

## Freshness algorithm

For a `FreshnessQuery` against a `rustc` entry, the entry is fresh iff:

1. The entry exists in the manifest under `key`.
2. The recorded `argv_hash` matches the query (same rustc flags,
   environment, cwd).
3. The declared `output_path` still exists on disk.
4. The recorded source set exactly matches the query's source set.
   Adding or removing a source file is always a change, even if every
   remaining file's hash is unchanged.
5. Every parent-directory mtime in the entry matches the filesystem.
   A directory mtime change indicates files were added or removed —
   this catches new sources that wouldn't appear in the per-file check.
6. Every source file passes the **two-tier freshness check**.

`CompositeQuery` follows the same shape minus step 2, plus an
`aux_hash` equality check.

### Two-tier source check

For each source file:

- **Fast path.** Recorded `mtime_ns` + `size` match the filesystem.
  Common case; runs in O(n) stats with no hashing.
- **Slow path (fallback).** When the fast path disagrees, compute the
  current SHA-256 and compare to the recorded one.
  - If they match, the mtime is a false positive (`touch`, CI checkout,
    `git restore`); refresh the mtime in place and keep the cache.
  - If they differ, the content genuinely changed → stale.

The slow path is what makes the cache robust against build systems
and CI setups that churn mtimes without changing content. It only
triggers when the fast path already disagrees, so the common happy
case never hashes anything.

### Eager fingerprint population

`mark_built` computes and stores SHA-256 for every source file
immediately, not lazily on first mtime mismatch. Lazy population
would mean *first touch triggers a rebuild that still doesn't
populate the hash, so the second touch also rebuilds*: the fallback
would never actually kick in. Eager hashing costs one full read per
build, which is dwarfed by the compile itself.

## Atomic persistence

`CacheManifest::save_atomic` writes the manifest using
write-to-temp + fsync + rename so a crash mid-write can never leave
a truncated manifest on disk:

- Temp filename is `<path>.tmp.<pid>.<nonce>`. The `<pid>.<nonce>`
  suffix prevents concurrent gluon processes that share the same
  build directory from clobbering each other's temp files. `<pid>`
  alone is not collision-free across PID namespaces (two containers
  can legitimately share a PID), so we append a nanosecond nonce
  drawn from the wall clock.
- On any failure during write/fsync/rename, the temp file is removed
  before the error is propagated. Without this cleanup, a cache
  directory would slowly accumulate `.tmp.*` orphans across repeated
  failed builds.
- Two processes writing the *final* path concurrently is still a race,
  but a last-writer-wins rename is acceptable: both writers are
  working from an internally-consistent snapshot.

For multi-process serialisation across the manifest read+write window,
see [Concurrency](./concurrency.md).

## Corruption policy

A corrupted or version-mismatched manifest is **always** treated as a
cold cache, never as a build failure. Specifically, on any of: file
missing, file unreadable, JSON parse failure, or version mismatch,
the loader returns a fresh default manifest plus warnings.

The rationale: a cache that refuses to load stops a build that was
otherwise going to succeed, which is strictly worse than a cache that
forces a rebuild. Warnings go to stderr so the user isn't silently
paying the rebuild cost, but `Cache::load` never returns an error.

## Schema versioning

`CacheManifest::CURRENT_VERSION` gates forward-compatibility:

- **v1** — single `entries: BTreeMap<String, CacheEntry>` map.
- **v2** (current) — split into `rustc` + `composite`; `CacheEntry`
  renamed to `RustcEntry`. v1 manifests are *discarded* on load
  (cold-cache fallback) — writing a converter wasn't worth the
  complexity for a build cache that re-populates on next run.

Both `rustc` and `composite` are tagged `#[serde(default)]`. This is
load-bearing for the v1→v2 migration UX: a v1 manifest has only
`entries: {...}` and no `rustc` key. Without the attribute, serde
would reject the document with a parse error and the user would see
"cache manifest is corrupted" instead of the cleaner "cache schema
upgraded; rebuilt clean" message.

When bumping schema versions: update `CURRENT_VERSION`, ensure the
load path still treats older versions as cold-cache (not as an error),
and document the migration in the changelog.

## Composite `aux_hash`

The composite-entry `aux_hash` is a single 32-byte digest covering
non-file inputs. The `composite_aux_hash` helper produces it as a
domain-separated SHA-256 over length-prefixed segments:

```
SHA-256(b"gluon.cache.composite.v1\0"
        || u64_le(num_segments)
        || u64_le(len(seg1)) || seg1
        || u64_le(len(seg2)) || seg2
        || ...)
```

The length prefix is what prevents `["foo", "bar"]` from colliding
with `["foob", "ar"]`.

The algorithm is **frozen**. Bumping the domain tag would invalidate
every existing composite cache entry's `aux_hash` and force a full
rebuild for that entry kind.

Typical aux-hash payloads:

- **sysroot:`<target>`** — the rustc version-hash string. Catches
  toolchain commits that share the same on-disk file fingerprint.
- **image:`<name>`** — serialised `ImageDef` bytes + `hadris-fat`
  crate version + per-entry source-kind tags.
- **esp:`<name>:<dest>`** — serialised `EspDef` entry bytes.

The `aux_hash` is opaque to the cache layer; only the producer
understands the segments, and only equality is ever checked.

## rustc dep-info inputs

Rustc's `--emit=dep-info` produces a Makefile fragment whose first
target-line lists every source file the crate compiled. Gluon parses
that file (see `cache::depinfo`) into the `sources` slice for
`FreshnessQuery` / `BuildRecord`. The parser only handles the subset
of Make syntax rustc actually emits — see
[external dependencies](./external-deps.md) for the format details.

## Canonical rustc-flag token order

The cache key for a rustc compile is `hash_argv` over the assembled
argv. Any reordering of the flag set changes the hash and invalidates
every existing cache entry, so the order is fixed:

```
input → crate-name → crate-type → edition → sysroot? → target? →
out-dir → emit → implicit-externs? → explicit-externs → opt-level →
debug-info → lto? → cfg-flags → panic-strategy? → extra-rustc-flags →
incremental → linker-script? → extra-filename
```

`?` marks tokens that appear only when applicable (host crates skip
`sysroot`/`target`, host build steps skip `linker-script`, etc.).

Keeping this order stable is **non-negotiable**: any reordering
invalidates every on-disk cache entry. Adding a new flag means
adding it at the end of the order, never inserting it.

## Composite-entry call sites

Composite entries are written by:

- **Sysroot** (`sysroot::ensure_sysroot`) — key `sysroot:<target_name>`,
  inputs `[rustc_path]`, aux from `RustcInfo::version_hash`.
- **ESP** (`compile/esp.rs`) — key `esp:<name>:<dest>`, one entry per
  ESP entry destination path.
- **Image** (`image::build_image`) — key `image:<name>`.

The `output_path` of each entry doubles as a "the artefact is still
intact" sentinel: `rm -rf <output_path>` forces a rebuild even if
nothing else has changed.

## When to add an entry kind

Adding new cached artefacts: prefer extending the existing manifest
with another composite-entry call site. The cache layer is the only
freshness system in gluon — there is no separate stamp file or
fingerprint file mechanism. If a stamp file exists in a build
directory, it is a leftover from before the unification (Stage 5);
the loader does not read it.
