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

## In-process: scheduler snapshot

Inside a single build, the scheduler runs many rustc compiles in
parallel via the `WorkerPool`. The compile artefacts they produce
need to be visible to subsequent stages (e.g. an `Image` node needs
the ESP path produced by an earlier `Esp` node).

Stage 8 collapsed four independent `Mutex`-guarded maps
(`sysroots`, `artifacts`, `esps`, `images`) into one
`Mutex<ArtifactsSnapshot>`:

```rust
struct ArtifactsSnapshot {
    sysroots:  BTreeMap<Handle<TargetDef>, PathBuf>,
    artifacts: ArtifactMap,
    esps:      BTreeMap<Handle<EspDef>, PathBuf>,
    images:    BTreeMap<Handle<ImageDef>, PathBuf>,
}
```

### Dispatch protocol

Each per-node dispatch arm in the scheduler follows this protocol:

1. **Lock** the `Mutex<ArtifactsSnapshot>`, **clone** the snapshot,
   **drop** the lock.
2. **Run** the per-node helper with `&ArtifactsSnapshot` (immutable,
   owned clone). The helper compiles, formats, or otherwise produces
   work output.
3. **Lock** the mutex again, **merge** the helper's `NodeOutput`
   into the canonical state via `ArtifactsSnapshot::merge`, **drop**
   the lock.

The lock is held for two short windows — clone and merge — never
across the helper itself. Helpers are pure functions over an
immutable view; they cannot deadlock against each other or against
the scheduler.

The dispatch arm is the **sole mutation point** for the canonical
snapshot. Per-node helpers never see a `Mutex`.

### Why a single bundle and not finer-grained locks?

Pre-Stage-8, each map had its own `Mutex<BTreeMap<…>>`. Helpers
locked individually as they needed paths. This produced two
problems:

- **Lock-across-helper.** Some helpers needed multiple maps and
  held locks while compiling — risky and a future deadlock waiting
  to happen.
- **Argument plumbing.** Each helper signature carried four
  `&Mutex<…>` fields. Adding a new artefact kind required threading
  another mutex through every dispatch arm.

A single `Mutex<ArtifactsSnapshot>` is conceptually simpler, has
one lock for workers to contend on (which is fine — each helper
takes the lock for nanoseconds, not for the duration of a compile),
and centralises the merge logic in `ArtifactsSnapshot::merge`.

### Snapshot rebuild cost

Cloning the snapshot at each dispatch is `O(n)` in the number of
artefacts. Current artefact-map sizes are well under 100 entries
even on big projects, so this is dominated by rustc spawn cost. If
profiling ever shows the clone dominating, the next step is
`Arc<RwLock>` reads with copy-on-write semantics — but this is
unlikely to be needed.

## Mutex poisoning

The scheduler's mutexes use `lock_or_poison` to convert a poisoned
lock into `Error::Config` rather than panicking. Mutex poisoning
happens when a thread panics while holding the lock; without
recovery, every subsequent `lock()` would re-panic. Converting to
an error lets the build fail cleanly with a diagnostic.

This is defensive only — in practice the helpers should never panic
while holding the lock (they hold it only across `Vec`/`BTreeMap`
inserts), but the conversion is cheap and the alternative
(uncatchable abort) is bad for users.

## Determinism interaction

Concurrency does not break determinism — see
[determinism architecture](./determinism.md). At `-j1`, dispatch
order is fully deterministic. At `-j > 1`, *artefact bytes* are
identical across runs but completion order is not.

The snapshot mechanism preserves determinism: each helper sees a
stable point-in-time view of upstream outputs, so its rustc argv
(and therefore its cache key) is a deterministic function of which
DAG dependencies completed before it ran — and the DAG itself is
deterministic.
