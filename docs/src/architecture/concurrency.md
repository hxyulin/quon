# Concurrency

Gluon runs in two concurrent dimensions:

- **Multi-process.** Two `gluon build` invocations against the same
  project can race on the shared build directory. A scripted
  editor-on-save, a terminal rebuild, and a CI pre-commit hook can
  all hit the same project simultaneously.
- **In-process.** A single `gluon build -j>1` runs many rustc
  invocations in parallel, each checking and writing its own
  fingerprint records.

Each dimension uses a different mechanism. Together they keep the
records consistent and per-node helpers free of shared mutable state.

## Multi-process: advisory flock

`CacheLock` is an advisory exclusive lock on `<build_root>/.lock`,
held for the entire duration of a build. The file is never read or
written.

`CacheLock::acquire` is called once at the top of `build` and `clean`,
held until the guard drops. While the lock is held, no other gluon
process in the same project can write outputs or records. Concurrent
builds serialise on the lock instead of clobbering each other. One
lock covers every driver, because `gluon check`, `gluon clippy` and
`gluon build` share the sysroot, the fixed-host-profile units and the
record directory.

### Acquisition strategy

Acquire is **non-blocking first, blocking fallback**:

1. Try `try_lock_exclusive`. If we get the lock, proceed silently —
   the common single-process case has no log noise.
2. If `WouldBlock`, print a one-line `gluon: waiting for another
   build in <path>` to stderr, then fall back to a blocking
   `lock_exclusive`.

The notice prevents the user from staring at a frozen terminal
wondering why nothing is happening — the most common confusing
failure mode of locking systems.

### Why a separate lockfile?

Records are replaced by an atomic rename (see
[cache architecture](./cache.md#records-on-disk)), so a lock on a
record file would be lost at the rename. A separate lockfile is also a good place to add liveness tags later
(PID, hostname, "this build is still running") if we ever want
richer wait diagnostics. For now the file is just a coordination
point — its contents are never read or written.

### Why not a process-internal mutex?

In-process mutexes don't help across `gluon` invocations from a
script-on-save editor + a terminal rebuild + a CI pre-commit hook.
Every common multi-build scenario is multi-process.

### Build vs clean

Both `build` and `clean` take this lock. Clean racing a build is
just as dangerous as build-vs-build: clean would `rm -rf` files the
builder is in the middle of writing.

### Lockfile lifecycle

The lockfile lives in the build root, so `gluon clean` (which removes
the build dir) takes it down too. The held `File` is intentionally private — there's
nothing to read or write inside it; the file exists purely as a
stable inode for the kernel's lock table.

`Drop` for `CacheLock` calls `unlock` best-effort; closing the file
descriptor would also drop the lock, so a failure to explicitly
unlock is harmless.

## Atomic record writes

Each record is written to `<file>.tmp.<pid>.<nonce>` and renamed into
place, so a reader never sees a partial record. There is no fsync: after
a crash a record is the old one, the new one, or unparsable, and an
unparsable record is stale, so the node reruns. The flock prevents
concurrent build logic; the rename is the last-mile defence if the
flock is somehow bypassed.

## In-process: result slots

Inside a single build, the scheduler runs many rustc compiles in
parallel via the `WorkerPool`. The outputs they produce must be
visible to later nodes (an `Image` node needs the ESP directory an
earlier `Esp` node assembled).

The dispatcher holds one `OnceLock<NodeResult>` per node, indexed by
`NodeId`:

```rust
struct NodeResult {
    primary: Option<PathBuf>, // rlib, binary, sysroot dir, ESP dir, image, ...
    was_cached: bool,
}
```

### Dispatch protocol

1. The worker runs the node's helper. The helper reads the outputs of
   its inputs through a `ResultView`, which looks a crate handle,
   target, ESP or image up in the slots.
2. When the helper succeeds, the dispatch arm writes the node's own
   slot. A slot is written once; a second write is a scheduler bug and
   fails the node.

No lock is involved. A node is dispatched only after all of its DAG
inputs finished, and `OnceLock` publishes a written value to every
thread that reads it afterwards, so a node never reads a slot that is
still being written. Each lookup is a map probe, not a copy, so a build
does O(1) work per input instead of copying every output so far at
each dispatch.

`BuildSummary` (built and cached counts, ESP, image and build-step
paths) is assembled from the slots after the pool joins.

### Failures

On a failure the pool stops dispatching and waits for in-flight jobs.
Each failure is labelled with the node's name (`crate kernel
(x86_64-unknown-none, dev)`, `sysroot x86_64-unknown-none`) and the
list is sorted by `NodeId`, so two simultaneous failures are reported
in the same order whichever finished first. Job output is still
printed live, in completion order.

Each node writes its own record file when it finishes, after its
outputs are complete. A failed or interrupted build therefore keeps
the records of every node that finished, and those nodes do not
rebuild next time.

## No shared locks

No in-process mutex guards the cache. Each node reads and writes only
its own record files (an ESP node owns one per entry), and keys are
unique per node, so two workers never touch the same file.

## Determinism interaction

Concurrency does not break determinism — see
[determinism architecture](./determinism.md). At `-j1`, dispatch
order is fully deterministic. At `-j > 1`, *artefact bytes* are
identical across runs but completion order is not.

The result slots preserve determinism: a helper reads only the slots
of its DAG inputs, which are all set before it runs, so its rustc argv
(and therefore its cache key) depends only on the DAG, which is itself
deterministic.
