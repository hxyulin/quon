# Contributing to Gluon

## Getting started

```bash
# Clone and enter the repo
git clone https://github.com/hxyulin/gluon.git
cd gluon

# The nightly toolchain + components are pinned in rust-toolchain.toml —
# rustup will install them automatically on the first cargo invocation.

# Run the full test suite
cargo test --workspace

# Run fmt and clippy (CI enforces both)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Project structure

| Crate | Purpose |
|-------|---------|
| `gluon-config` | Rust API that `gluon.rs` build scripts use to declare a project |
| `gluon-model` | Data types shared across the build pipeline |
| `gluon-core` | Engine, compiler, scheduler, cache, vendor, kconfig |
| `gluon-cli` | `clap`-based CLI (`gluon build`, `run`, `vendor`, ...) |

## Commit messages

Follow [Conventional Commits](https://www.conventionalcommits.org/),
for example `fix(compile): handle empty sysroot`. The scope is optional.
The `commit-msg` hook checks the format.

## Pull requests

- Keep PRs focused — one logical change per PR
- Ensure `cargo test --workspace`, `cargo fmt --check`, and
  `cargo clippy -- -D warnings` all pass before opening
- Add or update tests for any new or changed behavior
- Update documentation (mdbook pages, doc comments) if the user-facing
  surface changes

## Design guidelines

Gluon is a **kernel-agnostic** build system. Contributions should:

- Not bake assumptions about a specific kernel, target triple, or
  bootloader into core code — that belongs in `gluon.rs`
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
for inclusion in Gluon, as defined in the Apache-2.0 license, is dual
licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
without any additional terms or conditions.
