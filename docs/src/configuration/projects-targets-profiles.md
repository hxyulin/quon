# Projects, Targets, and Profiles

Every `gluon.rs` file declares at least one project, one target, and one
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
- **`.default_profile(name)`** -- the profile used when no `-p` flag is passed
  to the CLI. If omitted, Gluon requires an explicit profile selection.
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
  handle to it. Gluon will compile a custom sysroot (`core`, `alloc`,
  `compiler_builtins`) for each registered target.
- **`cfg.target_spec(name, path)`** -- like `target()`, but the second argument
  is a path to a custom target-spec JSON file, relative to `gluon.rs`. Use this
  for targets that are not built into rustc.
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
    .boot_binary("kernel");

cfg.profile("release")
    .inherits("dev")
    .opt_level(2)
    .lto("thin");
```

### Profile methods

| Method | Description |
|---|---|
| `.target(handle)` | Which registered target to build for (pass a target handle) |
| `.opt_level(n)` | Optimization level: 0, 1, 2, or 3 |
| `.debug_info(bool)` | Include debug symbols in the output |
| `.lto(mode)` | Link-time optimization: `"thin"`, `"fat"`, or `"off"` |
| `.boot_binary(crate_name)` | Which crate is the QEMU boot entry point |
| `.inherits(profile_name)` | Inherit settings from another profile |
| `.preset(name)` | Apply a named config preset |
| `.config(key, value)` | Set a config option value for this profile |

### Profile inheritance

The `.inherits(profile_name)` method creates a parent-child relationship
between profiles. The child profile inherits all settings from the parent, then
overrides any settings it explicitly sets.

In the example above, `release` inherits everything from `dev` (target,
debug_info, boot_binary) and overrides `opt_level` and `lto`. Unset fields fall
through to the parent.

Inheritance chains are resolved at configuration resolution time. A profile can
only inherit from one parent, but chains can be multiple levels deep
(`release` -> `dev` -> some base profile).
