# Concurrency

Gluon runs in two concurrent dimensions:

- **Multi-process.** Two `gluon build` invocations against the same
  project can race on the shared cache manifest. A scripted
  editor-on-save, a terminal rebuild, and a CI pre-commit hook can
  all hit the same project simultaneously.
- **In-process.** A single `gluon build -j>1` runs many rustc
  invocations in parallel, all consuming and producing entries in
  the in-memory `Cache`.

Each dimension uses a different mechanism. Together they keep the
cache consistent, the manifest uncorrupted, and per-node helpers
safe from shared mutable state.

## Multi-process: advisory flock

`CacheLock` is an advisory exclusive lock held on a sibling lockfile
for the entire duration of a build:

```
<build_root>/cache-manifest.json       # the actual manifest
<build_root>/cache-manifest.json.lock  # the lockfile (never read)
```

`CacheLock::acquire` is called once at the top of `build` and `clean`,
held until the guard drops. While the lock is held, no other gluon
process in the same project can read+modify+write the manifest.
Concurrent builds serialise on the lock instead of clobbering each
other.

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

### Why advisory and not the manifest itself?

We need to lock *across* the read and the write, with the manifest
file being deleted/recreated by the atomic rename in between (see
[cache architecture](./cache.md#atomic-persistence)). A lock on the
manifest itself would be invalidated the moment `save_atomic` did
its rename.

A separate lockfile is also a good place to add liveness tags later
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

The lockfile path is `<cache-manifest>.lock` so it's obviously a
sibling and `gluon clean` (which removes the build dir) takes it
down too. The held `File` is intentionally private — there's
nothing to read or write inside it; the file exists purely as a
stable inode for the kernel's lock table.

`Drop` for `CacheLock` calls `unlock` best-effort; closing the file
descriptor would also drop the lock, so a failure to explicitly
unlock is harmless.

## Atomic manifest writes

Multi-process safety also depends on atomic persistence. See
[cache architecture](./cache.md#atomic-persistence) for the full
write-temp + fsync + rename protocol. Two key properties:

- **Truncation safety.** A crash mid-write can never leave a
  truncated manifest. Worst case is the temp file is left orphaned;
  a successful subsequent build cleans it up.
- **Concurrent-temp safety.** The `<pid>.<nonce>` suffix on temp
  filenames means concurrent processes (which the flock makes
  impossible-in-practice but possible-in-theory if the lockfile is
  bypassed or removed) can't clobber each other's writes.

The flock and the atomic rename are layered: the flock prevents
concurrent build logic; the atomic rename is the last-mile defence
against truncation if the flock is somehow defeated.

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

`build_with_workers` saves the cache manifest after a failed build as
well. A node records itself in the in-memory cache only after its
outputs are complete, so the records of nodes that finished stay valid
and those nodes do not rebuild next time.

## Mutex poisoning

The one in-process mutex left is the cache's (`CompileCtx::cache`).
Callers convert a poisoned lock into `Error::Config("cache mutex
poisoned")` rather than panicking, so a panic in one worker fails the
build with a diagnostic instead of aborting. The lock is held only
across in-memory record reads and writes, never across a rustc spawn.

## Determinism interaction

Concurrency does not break determinism — see
[determinism architecture](./determinism.md). At `-j1`, dispatch
order is fully deterministic. At `-j > 1`, *artefact bytes* are
identical across runs but completion order is not.

The result slots preserve determinism: a helper reads only the slots
of its DAG inputs, which are all set before it runs, so its rustc argv
(and therefore its cache key) depends only on the DAG, which is itself
deterministic.
