# Projects, targets and profiles

## Project

```rust,ignore
q.project("my-kernel", "0.1.0");
```

`q.project(name, version)` names the project. Quon uses the name and
version in build output paths and diagnostics. The generated config
crate is `<name>_config`, for example `my_kernel_config` for a project
named `my-kernel`. See [Config Options and Kconfig](./config-options.md).

`q.default_profile(&profile)` picks the profile Quon uses when neither
`-p` nor `-r` is given. Without it, Quon uses `dev`:

```rust,ignore
let debug = q.add(Profile::new("debug").opt_level(1).debug_info(true));
q.default_profile(&debug);
```

## Targets

```rust,ignore
let none = Target::builtin("x86_64-unknown-none");
let firmware = Target::builtin("thumbv7em-none-eabihf").build_std();
let custom = Target::spec("riscv64gc-custom", "targets/riscv64gc-custom.json");
let host = Target::host();
```

A `Target` is a plain value. Declaring one does nothing on its own:
Quon registers it the first time a package, crate, dependency or
`q.default_target` uses it.

- `Target::builtin(triple)` is a target rustc knows, such as
  `x86_64-unknown-none`. Its crates link the toolchain's prebuilt
  `core`, `alloc` and `compiler_builtins`, as with Cargo, so the
  target must be installed: `rustup target add <triple>`.
- `.build_std()` on a builtin target makes Quon build those three
  crates from the `rust-src` component instead. Use it for a target
  rustup does not ship. It costs a one-time build (11s for
  `thumbv7em-none-eabihf`), and no other crate of the target can start
  before it ends.
- `Target::spec(name, path)` is a custom target defined by the JSON
  spec at `path`, relative to the project root. It has no prebuilt
  libraries, so Quon always builds its sysroot. rustc accepts a JSON
  spec only on nightly.
- `Target::host()` is the machine Quon runs on. Crates built for it
  link the host's standard library, and Quon builds no sysroot for
  them.

Each `Package`, `Crate` and `Dependency` builds for its own target.
Name it with `.target(&t)`, or set the common one once:

```rust,ignore
q.default_target(&Target::builtin("x86_64-unknown-none"));
let kernel = q.add(Package::new("kernel"));        // x86_64-unknown-none
let boot = q.add(Package::new("bootloader")
    .target(&Target::builtin("x86_64-unknown-uefi")));
```

A package or crate without `.target(..)` builds for
`q.default_target`, else for the host. A `Dependency` without
`.target(..)` builds for the host.

## Profiles

A profile says how crates are compiled, like a Cargo profile. Select
one with `quon -p <name>`, or `-r` for `release`.

Every project has a `dev` and a `release` profile:

| Profile | Settings |
|---|---|
| `dev` | opt-level 0, debug info |
| `release` | inherits `dev`, opt-level 3, no debug info |

Adding a profile with one of these names replaces the built-in one.
`Profile::dev()` and `Profile::release()` start from the built-in
settings:

```rust,ignore
q.add(Profile::release().lto(Lto::Thin));

let dev = q.profile("dev");
q.add(Profile::new("debug-log").inherits(&dev).preset(&verbose));
```

`q.profile(name)` returns a `ProfileRef` to a profile by name, which
is how a script refers to the built-in `dev` and `release`. Quon
reports an unknown name when it checks the model.

### Profile methods

| Method | Description |
|---|---|
| `.opt_level(n)` | rustc's `-C opt-level`: 0 to 3, `"s"` or `"z"` |
| `.debug_info(bool)` | Include debug info |
| `.lto(Lto::Thin)` | Link-time optimisation: `Lto::Off`, `Lto::Thin` or `Lto::Fat` |
| `.preset(&preset)` | Apply a [preset](./config-options.md#presets) of config option values |
| `.inherits(&profile)` | Take every unset setting from `profile` |
| `.runner(&runner)` | Run with this runner instead of the one `q.run` names |

A profile from `Profile::new(name)` sets nothing. Its unset settings
come from the profile it inherits, else opt-level 0, no debug info and
no LTO. rustc turns debug assertions and overflow checks off above
opt-level 0, as in Cargo's release profile.

### What `quon run` starts

`q.run(&artifact)` names the binary `quon run` starts and boot tests
boot, for every profile. `.runner(&runner)` names the
[runner](../running/runners.md) that starts it:

```rust,ignore
let kernel = q.add(Package::new("kernel"));
let qemu = q.add(Qemu::new("qemu-system-x86_64").machine("q35"));
q.run(&kernel).runner(&qemu);
```

Without `.runner(..)`, `quon run` runs a host binary directly. A
profile's own `.runner(&r)` replaces the run entry's runner for that
profile and the profiles that inherit from it, for example to boot
the same kernel under BIOS and UEFI:

```rust,ignore
let uefi = q.add(Qemu::new("qemu-system-x86_64").boot(BootMode::Uefi));
let bios = q.add(Qemu::new("qemu-system-x86_64").boot(BootMode::Bios));
q.run(&kernel).runner(&uefi);
let dev = q.profile("dev");
q.add(Profile::new("bios").inherits(&dev).runner(&bios));
```

`quon -p bios run` then boots under BIOS.

### The target profiles build for

All profiles build for one target. Quon picks it in this order:

1. The target of the artifact `q.run` names.
2. `q.default_target`.
3. The one non-host target that the project's packages, crates and
   dependencies use.
4. The host.

A project whose crates use several non-host targets, with no `q.run`
and no `q.default_target`, is an error. The error lists the targets.

### What a profile builds

Without `q.run`, a profile builds every crate. With it, the profile
builds every crate of its own target, the run binary, and what they
depend on. Crates of other targets build only when something the
profile builds needs them, for example through `artifact_env`.

### Host profiles

When the profiles build for the host, they need no declared target,
and build no sysroot and no config crate. Their crates link the host's
standard library, as under Cargo:

```rust,ignore
q.cargo_workspace(".");
let tool = q.add(Package::new("my-tool"));
q.run(&tool);
```

With no runner named, `quon run` runs the binary directly, with the
arguments after `--`, and passes when it exits with status 0. An ESP
or a QEMU runner needs a declared target, so a host profile reports an
error for either.

### Profile inheritance

`.inherits(&parent)` makes a profile take every setting it leaves
unset from `parent`. A profile inherits from one parent, and chains
can be several levels deep (`release` -> `dev` -> a base profile).

The target, the run binary and the run entry's runner are set on the
root of each chain, the profile that inherits nothing. Profiles that
inherit take them from it.
