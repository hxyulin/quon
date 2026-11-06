# Contributing to Quon

## Getting started

```bash
# Clone and enter the repo
git clone https://github.com/hxyulin/quon.git
cd quon

# The nightly toolchain + components are pinned in rust-toolchain.toml —
# rustup will install them automatically on the first cargo invocation.

# Run the test suite
cargo test --workspace

# Also run the end-to-end tests, which spawn rustc
cargo test --workspace -- --include-ignored

# The same with cargo-nextest, which runs test binaries in parallel
# (`cargo install cargo-nextest`). nextest skips doctests.
cargo nextest run --workspace --run-ignored all
cargo test --workspace --doc

# Run fmt and clippy (CI enforces both)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Project structure

| Crate | Purpose |
|-------|---------|
| `quon-config` | Rust API that `quon.rs` build scripts use to declare a project |
| `quon-model` | Data types shared across the build pipeline |
| `quon-core` | Engine, compiler, scheduler, cache, vendor, kconfig |
| `quon-runner` | Runner plugin protocol: request and reply types, `serve()` |
| `quon-runner-qemu` | The QEMU runner plugin: argv, boot modes, OVMF |
| `quon` | `clap`-based CLI (`quon build`, `run`, `vendor`, ...) |

## Commit messages

Follow [Conventional Commits](https://www.conventionalcommits.org/),
for example `fix(compile): handle empty sysroot`. The scope is optional.
The `commit-msg` hook checks the format.

## Tests

Tests that spawn rustc are `#[ignore]`d, so `cargo test --workspace`
finishes in seconds. `-- --include-ignored` runs them too and needs the
`rust-src` component. `cargo test` runs one test binary at a time, so
the end-to-end binaries wait on each other; `cargo nextest run` runs
every test as its own process and takes about half as long. Most of them copy a prebuilt sysroot into their
fixture instead of building `core`, `compiler_builtins` and `alloc`
each time (see `crates/quon-core/src/testing.rs`). The prebuilt
sysroots live in `$TMPDIR/quon-test-sysroots/`, about 120 MB per target
and toolchain; delete the directory to reclaim the space.

## Pull requests

- Keep PRs focused — one logical change per PR
- Ensure `cargo test --workspace`, `cargo fmt --check`, and
  `cargo clippy -- -D warnings` all pass before opening
- Add or update tests for any new or changed behavior
- Update documentation (mdbook pages, doc comments) if the user-facing
  surface changes

## Design guidelines

Quon is a **kernel-agnostic** build system. Contributions should:

- Not bake assumptions about a specific kernel, target triple, or
  bootloader into core code — that belongs in `quon.rs`
- Prefer deterministic, reproducible behavior (stable ordering,
  no reliance on `HashMap` iteration order)
- Surface errors with context — build failures should point the user at
  the offending crate, file, or config option
- Use `unsafe` only when strictly necessary, isolated behind safe APIs

## Documentation

The mdbook documentation lives in `docs/src/`. To preview locally:

```bash
# Install mdbook if needed
cargo install mdbook

# Build and serve with live reload
cd docs
mdbook serve --open
```

## License

Unless you state otherwise, any contribution you intentionally submit
for inclusion in Quon, as defined in the Apache-2.0 license, is dual
licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
without any additional terms or conditions.
