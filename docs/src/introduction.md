# Introduction

Gluon is a declarative build system for Rust. It reads a Cargo
workspace, drives `rustc` directly with per-crate flags, and carries a
project from configuration through compilation to the binary running:
on the host, through a command, or under QEMU. The build is declared in
one Rust file, `gluon.rs`, next to the workspace's `Cargo.toml`.

![A clean gluon build and gluon test of the Hadron kernel](images/hadron-build.gif)

**What does it add to Cargo?** Cargo builds one target per invocation
and stops at the compiled binary. Gluon builds several targets in one
graph, compiles sysroots (`core`, `alloc`, `compiler_builtins`) for
targets that have none, and runs the steps after compiling: linker
scripts, file copies, image tools, ESP layout and QEMU runs. It also
has Kconfig-style typed config options, which Cargo features cannot
express.

**Does it replace Cargo.toml?** No. Gluon reads dependencies, features,
editions, lints and build scripts from the manifests on every build.
`gluon.rs` only says what Cargo cannot: targets, profiles, groups,
runners and post-build steps. `gluon init` writes one for an existing
project, and `cargo build` keeps working beside it.

**Who is it for?** Projects whose build goes past `cargo build`:
kernels, hypervisors, bootloaders and firmware, and also host tools and
libraries that want one command to build, run and test. Gluon builds
and tests itself from its own `gluon.rs`. It is kernel-agnostic: it
assumes no target triple, bootloader or boot protocol. It works on
stable Rust; only custom `.json` targets need nightly. The project is under active development; expect
rough edges and API changes.

Ready to try it? Head to [Installation](./getting-started/installation.md)
or [Generating gluon.rs](./getting-started/init.md).
