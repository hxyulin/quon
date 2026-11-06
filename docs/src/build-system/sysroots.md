# Custom Sysroots

## What is a sysroot?

The **sysroot** is the directory where rustc looks for `core`, `alloc`
and `compiler_builtins`. rustup ships them prebuilt for the targets it
supports, bare-metal ones such as `x86_64-unknown-none` and
`thumbv7em-none-eabihf` included. A custom target spec has no prebuilt
libraries, so they must be compiled from source.

## How Quon handles sysroots

A builtin target links the toolchain's prebuilt libraries, as Cargo
does. Quon only checks that the target is installed, and names
`rustup target add <triple>` when it is not.

Quon compiles a sysroot from the `rust-src` component for:

- every custom target (`Target::spec`);
- a builtin target declared with `.build_std()`, for a target rustup
  does not ship.

Building from source costs 11s once for `thumbv7em-none-eabihf`, and
no other crate of the target starts before it ends, so Quon does it only
when `quon.rs` asks.

The compiled sysroot lives at:

```
build/sysroot/<target>/
```

### What gets compiled

- **`core`** -- the foundational `#![no_std]` library. Every bare-metal
  crate depends on this.
- **`alloc`** -- heap allocation support (`Box`, `Vec`, `String`, etc.).
  Required if your kernel uses a global allocator.
- **`compiler_builtins`** -- compiler intrinsics such as `memcpy`,
  `memset`, and soft-float routines. Rustc expects these to be available
  for any target.

### When the sysroot is rebuilt

The sysroot is rebuilt when any of the following conditions are met:

- **First build** for a given target triple.
- **Rust toolchain change** -- detected by comparing the `rustc` binary's
  modification time against the cached value.
- **Missing or corrupted sysroot directory** -- if the directory does not
  exist or is incomplete, Quon rebuilds it from scratch.

In all other cases, the existing sysroot is reused.

## Requirements

The `rust-src` component must be installed for the active toolchain:

```sh
rustup component add rust-src
```

Quon locates the `rust-src` path by querying the active toolchain's
sysroot (`rustc --print sysroot`) and looking for
`lib/rustlib/src/rust/library/` underneath it.

If `rust-src` is not installed, Quon will report a clear error pointing
you at the `rustup component add` command.

## Sysroot caching

Compiling the sysroot is the most expensive part of a clean build -- it
takes significantly longer than compiling your own crates. Quon caches
it aggressively to avoid repeating this work.

The sysroot is **shared across profiles** (debug, release, test, etc.)
and **shared across drivers** (build, check, clippy). There is one sysroot
per target triple, not one per profile.

### Cleaning

- `quon clean` -- wipes the entire `build/` directory, including the
  sysroot.
- `quon clean --keep-sysroot` -- removes all build artifacts but
  preserves the sysroot. This is useful when you want a fresh build
  without paying the sysroot compilation cost again.
