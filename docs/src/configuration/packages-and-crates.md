# Packages and crates

Quon builds two kinds of crate:

- A `Package` is a package in the Cargo workspace that
  `q.cargo_workspace(dir)` names. Its dependencies, features, edition
  and lib and bin targets come from its `Cargo.toml`.
- A `Crate` is a crate with no `Cargo.toml`. Its sources, type and
  dependencies are all declared in `quon.rs`.

Prefer `Package` for anything in a Cargo workspace. Both are declared
with `q.add`, which returns an `Artifact` handle.

## Targets

Each package and crate carries its own target. `.target(&t)` sets it,
and `q.default_target(&t)` sets it for every package and crate that
names none:

```rust,ignore
let none = Target::builtin("x86_64-unknown-none");
let uefi = Target::builtin("x86_64-unknown-uefi");
q.default_target(&none);

let kernel = q.add(Package::new("kernel"));                   // x86_64-unknown-none
let boot = q.add(Package::new("bootloader").target(&uefi));   // x86_64-unknown-uefi
let tool = q.add(Package::new("mkimage").target(&Target::host()));
```

Without `q.default_target`, a package or crate with no `.target(..)`
builds for the host.

## Packages

```rust,ignore
q.cargo_workspace(".");

let kernel = q.add(
    Package::new("kernel")
        .target(&none)
        .linker_script("crates/kernel/kernel.ld")
        .features(["alloc"])
        .rustc_flag("-Crelocation-model=static"),
);
```

`quon.rs` holds only what Cargo has no concept of. A package without
`q.cargo_workspace` is an error.

| Method | Description |
|---|---|
| `.features([..])` | Enable features, like `cargo build --features` |
| `.no_default_features()` | Leave the `default` feature off |
| `.raw_binary()` | Turn the package's bins into flat binaries with `objcopy -O binary` |
| `.link_config()` | Link the generated config crate |

The handle `q.add` returns names the bin, or the lib, that has the
package's name. For a package with more than one `[[bin]]`, reach the
others with `Artifact::bin`:

```rust,ignore
let tools = q.add(Package::new("tools"));
let mkimage = tools.bin("mkimage");
q.run(&mkimage);
```

A `.raw_binary()` package needs a bin target in its `Cargo.toml`.

## Crates

```rust,ignore
let util = q.add(Crate::lib("util", "crates/util").edition(Edition::E2021));
let kernel = q.add(
    Crate::bin("kernel", "crates/kernel")
        .linker_script("crates/kernel/kernel.ld")
        .dep(&util)
        .cfg("my_flag"),
);
```

The path is the crate's directory, relative to the project root. The
constructor sets the crate type:

| Constructor | Crate type | Default root |
|---|---|---|
| `Crate::lib(name, path)` | library | `src/lib.rs` |
| `Crate::bin(name, path)` | binary | `src/main.rs` |
| `Crate::proc_macro(name, path)` | procedural macro, built for the host | `src/lib.rs` |
| `Crate::raw_binary(name, path)` | flat binary: an ELF copied out with `objcopy -O binary` | `src/main.rs` |
| `Crate::new(name, path, CrateType::StaticLib)` | any `CrateType` | `src/lib.rs` |

| Method | Description |
|---|---|
| `.root(path)` | The crate root, relative to the crate directory, when it is not the default |
| `.edition(Edition::E2021)` | The Rust edition. Defaults to `Edition::E2024` |
| `.dep(&artifact)` | Depend on a crate or a [`Dependency`](./dependencies.md) |

A crate declared with `Crate` links the generated config crate
whenever it links the sysroot.

### Dependencies between crates

`.dep(&handle)` takes the handle of a crate or a `Dependency`. The
crate uses it under the dependency's name with `-` replaced by `_`.
`Dep::on(&handle)` sets the options of a `[dependencies]` entry:

```rust,ignore
Crate::bin("app", "crates/app")
    .dep(Dep::on(&log).rename("logger").features(["serial"]));
```

| `Dep` method | Description |
|---|---|
| `.rename(name)` | The name the crate uses in `--extern` |
| `.features([..])` | Features to enable |
| `.version(v)` | Version requirement |
| `.optional()` | Link the dependency only when a feature enables it |
| `.no_default_features()` | Leave the `default` feature off |

## Settings shared by packages and crates

| Method | Description |
|---|---|
| `.target(&t)` | Build for `t` instead of the default target |
| `.linker_script(path)` | Link with the linker script at `path`, relative to the project root |
| `.cfg(flag)` | Pass `--cfg <flag>` to rustc. Call it once per flag |
| `.rustc_flag(flag)` | Pass `flag` to rustc, after the flags Quon sets. Call it once per flag |
| `.artifact_env(var, &artifact)` | See below |
| `.no_sysroot()` | Build without Quon's sysroot and config crate, for targets that cannot build `core`, `alloc` and `compiler_builtins` |

## Artifact environment variables

`.artifact_env(var, &artifact)` lets one crate embed another's output,
a common pattern for a bootloader and a kernel:

```rust,ignore
let kernel = q.add(Package::new("kernel").target(&none));
let boot = q.add(
    Package::new("bootloader")
        .target(&uefi)
        .artifact_env("KERNEL_PATH", &kernel),
);
```

It does two things:

1. It sets the environment variable `var`, at compile time, to the
   path of `artifact`'s output.
2. It builds `artifact` first.

`artifact` is a crate or a [build step](../build-system/build-steps.md).
The bootloader can then embed the kernel:

```rust,ignore
// In the bootloader crate
static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));
```
