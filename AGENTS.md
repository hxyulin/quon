# Working on Gluon

Gluon is a build system for bare-metal Rust. It drives `rustc` directly,
builds sysroots for custom targets, and takes a project from its `gluon.rs`
configuration to bootable images and QEMU runs. It is a host tool, so it
has no `no_std` or dependency-count constraints.

## How to collaborate

- Ask when a request is vague, ambiguous, or looks wrong. Don't guess the
  intent and build on the guess.
- When there is more than one reasonable approach, name the options and
  their trade-offs, then recommend one.
- Explain the reasoning behind build-system choices. Incremental builds,
  parallel scheduling, cache invalidation and dependency resolution all
  involve trade-offs that should be stated, not buried in code.
- Agree on a design before writing code. For anything beyond a small fix,
  describe the approach, the edge cases, and where it touches existing
  code, and wait for agreement.

## Engineering rules

1. **Stay kernel-agnostic.** Don't build assumptions about a particular
   kernel, target triple, bootloader or boot protocol into the core. That
   belongs in the project's `gluon.rs`.
2. **Use mature crates where they pay off.** `clap`, `serde` and similar
   are fine. Avoid trivial micro-crates and overlapping ecosystems.
3. **Keep builds deterministic.** The same `gluon.rs` and source tree
   must produce the same artifacts. Use stable iteration order (`BTreeMap`,
   sorted paths), never `HashMap` order or filesystem order.
4. **Never serve stale outputs.** When freshness is uncertain, rebuild.
   Document the freshness rule wherever a cache check happens.
5. **Avoid `unsafe`.** If it is needed, isolate it behind a safe API and
   document the invariants.
6. **Report errors with context.** A build failure names the crate, file,
   `rustc` invocation or config option at fault.
7. **Comment on why, not what.** Explain why code exists or why an
   approach was chosen, especially around caching, scheduling and `rustc`
   flag assembly.

## Checks

Run these before committing:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Commit messages follow the format in [CONTRIBUTING.md](CONTRIBUTING.md).
