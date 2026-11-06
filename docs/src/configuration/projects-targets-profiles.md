# Projects, Targets, and Profiles

Every `quon.rs` file declares at least one project, one target, and one
profile. These three entities establish the core identity and compilation
settings for your build.

## Project

```rust
cfg.project("my-kernel", "0.1.0")
    .default_profile("dev");

cfg.config_crate_name("my_kernel_config");
```

- **`cfg.project(name, version)`** -- the name and version are required. They are
  used in build output paths and diagnostics. Returns a `ProjectBuilder` for
  chaining `.default_profile()`.
- **`.default_profile(name)`** -- the profile used when neither `-p` nor `-r`
  is passed to the CLI. If omitted, Quon uses the first profile in
  alphabetical order.
- **`cfg.config_crate_name(name)`** -- overrides the name of the auto-generated
  config crate. The default is `<project>_config` (e.g., `my_kernel_config`
  for a project named `my-kernel`). See
  [Config Options and Kconfig](./config-options.md) for details on the config
  crate.

## Target

```rust
let t1 = cfg.target("x86_64-unknown-none");
let t2 = cfg.target("aarch64-unknown-none").panic_strategy("abort");
let t3 = cfg.target_spec("riscv64gc-custom", "./targets/riscv64gc-custom.json");
```

- **`cfg.target(triple)`** -- registers a rustc target triple and returns a
  handle to it. Quon will compile a custom sysroot (`core`, `alloc`,
  `compiler_builtins`) for each registered target.
- **`cfg.target_spec(name, path)`** -- like `target()`, but the second argument
  is a path to a custom target-spec JSON file, relative to `quon.rs`. Use this
  for targets that are not built into rustc. rustc accepts a JSON spec only
  on nightly.
- **`.panic_strategy("abort")`** -- sets the panic strategy for bare-metal
  targets. Common values are `"abort"` and `"unwind"`.

Built-in rustc targets (like `x86_64-unknown-none` or `aarch64-unknown-none`)
do not need a JSON spec. Custom targets -- for example, a modified RISC-V
triple -- require one.

## Profile

Profiles control how code is compiled. They are analogous to Cargo's `dev` and
`release` profiles but are more explicit and flexible.

```rust
let target = cfg.target("x86_64-unknown-none");

cfg.profile("dev")
    .target(target)
    .opt_level(0)
    .debug_info(true)
    .run_binary("kernel");

cfg.profile("release")
    .inherits("dev")
    .opt_level(2)
    .lto("thin");
```

### Profile methods

| Method | Description |
|---|---|
| `.target(handle)` | Which registered target to build for (pass a target handle) |
| `.target_host()` | Build for the host instead: a profile for a host tool |
| `.opt_level(n)` | Optimization level: 0, 1, 2, or 3 |
| `.debug_info(bool)` | Include debug symbols in the output |
| `.lto(mode)` | Link-time optimization: `"thin"`, `"fat"`, or `"off"` |
| `.run_binary(crate_name)` | The binary `quon run` starts with the profile's runner |
| `.runner(name)` | The [runner](../running/runners.md) that starts it |
| `.inherits(profile_name)` | Inherit settings from another profile |
| `.preset(name)` | Apply a named config preset |
| `.config(key, value)` | Set a config option value for this profile |

### What a profile builds

Without `.run_binary(...)`, a profile builds every crate. With it, the
profile builds every crate of its own target, the run binary, and what
they depend on. Crates of other targets build only when something the
profile builds needs them, for example through `artifact_env`. A project
with an x86_64 profile and an aarch64 profile therefore does not compile
the aarch64 crates for the x86_64 profile.

### Host profiles

A profile with `.target_host()` builds for the machine Quon runs on. It
needs no declared target, and builds no sysroot and no config crate:
its crates link the host's standard library, as under Cargo. Put its
packages in a group with `.target_host()`:

```rust,ignore
cfg.cargo_workspace(".");
cfg.group("tools").target_host().package("my-tool");
cfg.profile("dev").target_host().run_binary("my-tool");
```

With no `.runner(...)` named, `quon run` runs the binary directly,
with the arguments after `--`, and passes when it exits with status 0.
An ESP or a QEMU runner needs a declared target, so a host profile
reports an error for either.

### Profile inheritance

The `.inherits(profile_name)` method creates a parent-child relationship
between profiles. The child profile inherits all settings from the parent, then
overrides any settings it explicitly sets.

In the example above, `release` inherits everything from `dev` (target,
debug_info, run_binary) and overrides `opt_level` and `lto`. Unset fields fall
through to the parent.

Inheritance chains are resolved at configuration resolution time. A profile can
only inherit from one parent, but chains can be multiple levels deep
(`release` -> `dev` -> some base profile).
