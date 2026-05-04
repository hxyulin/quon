# Determinism

Gluon's correctness model is reproducibility-first: a given
`gluon.rs` plus source tree must always produce byte-identical
artefacts. This is more than a quality-of-life nicety — the build
cache hashes the rustc argv on every compile, and the argv is built
from in-memory data structures iterated at compile time. The instant
any of those iterations becomes nondeterministic, the cache key
becomes unstable and incremental builds fall apart.

This document describes the conventions that keep that contract
intact.

## The rule

> **Use `BTreeMap` and `BTreeSet`. Never `HashMap` or `HashSet` in
> code paths that contribute to a build artefact.**

`HashMap`'s iteration order is randomised across runs. The first
iteration of a `HashMap<&str, _>` over the same data on two
consecutive builds will yield two different orders, and every flag
push, every `--extern` argument, every diagnostic that enumerates
registered names follows that order. A single `HashMap` upstream of
the cache key is enough to guarantee 100% cache misses.

`BTreeMap`/`BTreeSet` iterate in sorted-key order, which is a
function of the key contents alone, not of insertion order or hash
seed. Two runs over identical inputs produce identical iterations.

## Where this matters

Every load-bearing collection in the build pipeline is a `BTree*`:

- **DAG adjacency** (`scheduler::dag`). Edges are stored as
  `BTreeMap<NodeId, BTreeSet<NodeId>>` so the topological dispatch
  order is the same on every run.
- **Artefact maps** (`compile::extern_map::ArtifactMap`).
  `--extern` flags are pushed in iteration order; the `BTreeMap` key
  is `Handle<CrateDef>` (a monotonically assigned integer), so the
  resulting argv is identical across runs on any machine.
- **Cache manifest** (`cache::manifest`). Source-file fingerprints
  use `BTreeMap<PathBuf, FileFingerprint>` so JSON serialisation is
  byte-stable.
- **DAG ready set** (`scheduler::worker`). At `-j1`, the ready set
  is a `BTreeSet<NodeId>`; the smallest ready node is popped each
  step, so dispatch order is fully deterministic — it always
  follows the ascending `NodeId` order. Tests that assert a specific
  ordering (e.g. topological correctness) use `workers=1` to get
  reproducible results.
- **Rule registry** (`rule::mod`). Even though handler lookup by
  name is not order-sensitive, future diagnostics that enumerate
  registered rules (e.g. "did you mean…?") must produce stable
  output.

## Edge dedup invariant

`scheduler::dag::Dag::edges_out` is a `BTreeMap<NodeId, BTreeSet<NodeId>>`,
specifically with `BTreeSet` (not `Vec`) values.

This is **not** purely an iteration-order concern. A previous
implementation used `Vec` and suffered a real scheduler deadlock:
when `add_edge` was called twice for the same `(from, to)` pair (a
common occurrence because multiple rules in a stage can reference the
same group), the in-degree of `to` was incremented twice but the edge
was only conceptually present once. The result was a node whose
in-degree could never reach zero — a permanent deadlock.

`BTreeSet` prevents this by construction: inserting the same edge
twice is a no-op, and `add_edge` increments in-degree only when the
set actually grows. **Do not change `edges_out` to `Vec` under any
circumstances.** The comment in `scheduler::dag` lives at the
violation site (the type declaration itself) — leave it inline.

## Cache-key stability

The rustc argv is the cache key. Determinism of the argv comes from
two layers:

1. **Stable iteration over `BTreeMap`.** `--extern` flags, `--cfg`
   flags, and explicit-extern crate paths all come from `BTreeMap`s
   keyed by handle or crate name.
2. **Stable token order.** The flag set is assembled in a fixed,
   documented order (input → crate-name → … → extra-filename). See
   the [cache architecture](./cache.md) for the canonical order.

Either layer breaking is a regression. Adding a new flag means
appending it to the canonical order *and* sourcing its values from a
`BTreeMap`/`BTreeSet`/`Vec`-with-stable-construction.

## Multi-threaded builds (`-j > 1`)

Determinism guarantees relax under parallelism but never break:

- **Per-job output is atomic.** Each worker writes into its own
  `Vec<u8>` for stdout and stderr; the main thread drains buffers to
  the sink in completion order. You never see rustc output from
  crate A interrupting crate B.
- **Job *completion* order is not deterministic.** The order in
  which jobs finish depends on rustc timing. The artefact set is
  still byte-identical (same compiles, same argvs, same outputs);
  only the surrounding console interleaving varies.
- **`-j1` is fully deterministic.** Dispatch order, output order,
  artefact bytes — all reproducible run-to-run. `gluon build -j1`
  is the recommended mode for CI artefact hashing or any context
  where reproducibility is asserted.

## Generated artefacts

Code generators (e.g. the `<project>_config` crate writer in
`generated::config_crate`) iterate config options in sorted key
order so the generated source is stable. The
`generate_source_is_deterministic` test pins this.

The same applies to anything that writes a file derived from the
build model: iterate the model with sorted iteration, and the file
contents will be reproducible.

## Practical guidance

- **Reach for `BTreeMap`/`BTreeSet` by default.** The microsecond
  cost of an ordered tree over a hash table is negligible compared
  to a single rustc invocation, and you don't have to think about
  whether this collection might one day end up upstream of the cache
  key.
- **If you need a `HashMap` for performance**, contain it tightly:
  build it, query it, drop it before producing any output. Never
  iterate a `HashMap` to build flags, paths, or diagnostics.
- **`Vec` is fine** when you control insertion order (e.g. parsing a
  list out of a config file in source order).
- **Snapshot tests pin determinism.** The integration tests under
  `crates/gluon-core/tests/` and `crates/gluon-cli/tests/` run a
  build twice and assert byte-identical artefacts; if you add a
  feature that touches a `BTreeMap`-shaped collection, make sure
  these still pass.

## Verifying determinism

```sh
gluon build -j1                      # first run
sha256sum target/gluon/**/*.rlib > a # capture
gluon clean
gluon build -j1                      # second run
sha256sum target/gluon/**/*.rlib > b
diff a b                             # must be empty
```

A failing diff means determinism has broken somewhere. Bisect by
`HashMap`/`HashSet` introduction or by changes to flag-assembly
order.
