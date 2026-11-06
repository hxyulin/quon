# From `Config` to `Quon`

The `quon_config::Config` API is replaced by `quon_config::Quon`. A
script written for `Config` fails to compile, and Quon prints a note
with the new form. This page maps the old calls to the new ones.

## What changed

- **Items are values passed to `q.add`.** Each `q.add` returns a typed
  handle (`Artifact`, `Runner`, `ProfileRef`, `PresetRef`, `OptRef`),
  and other declarations take the handle instead of a name string.
- **Groups are gone.** Each `Package` and `Crate` carries its own
  target. `q.default_target(&t)` sets it for every one that names
  none.
- **Profiles hold codegen settings only.** `dev` and `release` always
  exist unless the script declares them. What `quon run` starts is set
  once with `q.run(&artifact).runner(&runner)`, and profiles build for
  that artifact's target. See
  [The target profiles build for](../configuration/projects-targets-profiles.md#the-target-profiles-build-for).
- **Runners have no name.** A profile picks another runner with
  `Profile::runner(&r)`.

## Entry point

```rust,ignore
// Before
use quon_config::{Config, Result};
pub fn config(cfg: &mut Config) -> Result<()> { /* ... */ }

// After
use quon_config::prelude::*;
pub fn config(q: &mut Quon) -> Result<()> { /* ... */ }
```

In tests that build a model in process, `Config::new()` becomes
`Quon::new()`, and `q.into_raw()` returns a `Result` that fails on any
reported error. `q.finish()` returns the model and the errors.

## Mapping

### Project, targets and crates

| `Config` | `Quon` |
|---|---|
| `cfg.project(n, v).default_profile("debug")` | `q.project(n, v);` and `q.default_profile(&debug);` |
| `cfg.target("x")` | `Target::builtin("x")` |
| `cfg.target_spec(n, path)` | `Target::spec(n, path)` |
| `.target_host()` | `.target(&Target::host())` |
| `cfg.group(g).target(t).package(p)` | `q.add(Package::new(p).target(&t))` |
| group `.config(true)` | `Package::link_config()` |
| group `.no_sysroot()` | `.no_sysroot()` on the package or crate |
| group `.edition("2021")` | `Crate::edition(Edition::E2021)`. A package takes its edition from `Cargo.toml` |
| `group.add(n, path)` | `q.add(Crate::lib(n, path))` |
| `.add(n, path).crate_type(CrateType::Bin)` | `Crate::bin(n, path)`, `Crate::proc_macro`, `Crate::raw_binary`, or `Crate::new(n, path, ty)` |
| package `.crate_type(CrateType::RawBinary)` | `Package::raw_binary()` |
| `.dep("ext", "name")` | `.dep(&name)`, or `.dep(Dep::on(&name).rename("ext"))` |
| `.dep_with("a", "a", \|b\| { b.features(&["x"]); })` | `.dep(Dep::on(&a).features(["x"]))` |
| `.cfg_flag(x)` | `.cfg(x)` |
| `.artifact_env("K", "kernel")` | `.artifact_env("K", &kernel)` |
| package `.features(&["a"])` | `Package::features(["a"])` |
| `cfg.dependency("b").version("2.11").target(t)` | `q.add(Dependency::crates_io("b", "2.11").target(&t))` |
| `cfg.dependency(n).path(p)` / `.git(url)` | `Dependency::path(n, p)` / `Dependency::git(n, url)` |
| `cfg.cargo_workspace(".").vendored("v")` | `q.cargo_workspace(".").vendored("v")` |

### Profiles and runners

| `Config` | `Quon` |
|---|---|
| `cfg.profile("dev").target(t).opt_level(0).debug_info(true)` | Nothing: `dev` is built in. `q.default_target(&t)` if needed |
| `.run_binary("k").runner("qemu")` | `q.run(&k).runner(&qemu);` |
| `cfg.profile("debug").opt_level(0)` | `q.add(Profile::new("debug").opt_level(0))` |
| `.inherits("dev")` | `.inherits(&q.profile("dev"))` |
| `.lto("thin")` | `.lto(Lto::Thin)` |
| `.preset("x")` | `.preset(&x)` |
| `profile.qemu_boot_mode("bios")` | A second `Qemu` with `.boot(BootMode::Bios)`, named with `Profile::runner(&it)` |
| `cfg.runner("qemu").qemu(prog)` | `q.add(Qemu::new(prog))` |
| `.memory(n)` | `.memory_mb(n)` |
| `.serial_stdio()` / `.serial_none()` / `.serial_file(p)` | `.serial(Serial::Stdio)` / `Serial::None` / `Serial::File(p.into())` |
| `.boot_mode("uefi")` | `.boot(BootMode::Uefi)` |
| `.extra_args(&[..])` | `.args([..])` |
| `.ovmf_code(a).ovmf_vars(b)` | `.ovmf(a, b)` |
| `.disk_image(path)` / `.disk_image_ref(name)` | `.disk_file(path)` / `.disk(&image)` |
| `cfg.runner(n).command(&[..])` | `q.add(Command::new([..]))` |
| `cfg.runner(n).plugin(p).setting(k, v)` | `q.add(Plugin::new(p).setting(k, v))` |

### Files, options and tests

| `Config` | `Quon` |
|---|---|
| `cfg.esp(n).add("crate", d).add_file(p, d).add_download("limine/X", d)` | `q.add(Esp::new(n).add(&krate, d).add(p, d).add(&download, d))` |
| `cfg.limine(v).file(f, sha)` | `q.add(Download::limine(v, f, sha))` |
| `cfg.download(n, url, sha)` | `q.add(Download::new(n, url, sha))` |
| `cfg.image(n).format("fat32")` | `q.add(Image::new(n).format(ImageFormat::Fat32))` |
| `.add_entry_crate/file/download/esp(src, d)` | `.add(&artifact, d)`, or `.add("path", d)` for a file |
| `.partition(n, lba, fs).bootable().add(c, d).end()` | `.partition(Partition::new(n, lba, fs).bootable().add(&c, d))` |
| `.raw_write_crate(lba, c, max)` | `.raw_write(lba, &c, max)`; also a path string or a `Vec<u8>` |
| `cfg.build_step(n).runner("tool").input_crate(K, c).output(f)` | `q.add(BuildStep::new(n, f).tool(&tool).input(K, &c))` |
| `.input_literal(K, v)` | `.env(K, v)` |
| `cfg.config_bool(n).default_value(ConfigValue::Bool(true))` | `q.add(opt::bool(n).default(true))` |
| `config_u32/u64/str/tristate/list` | `opt::u32/u64/str/tristate/list` |
| `config_choice(n).choices(&[..])` | `opt::choice(n, [..])`. The first is the default |
| `.range(lo, hi)` | `.range(lo..=hi)` |
| `.depends_on(Expr::And(vec![Ident("A"), Not(Ident("B"))]))` | `.depends_on(&a & !&b)` |
| `.selects(&["X"])` | `.selects(&x)`, once per option |
| `cfg.preset(n).set("X", ConfigValue::U32(5))` | `q.add(Preset::new(n).set(&x, 5))` |
| `cfg.host_tests()` | `q.host_tests().package(a).packages([..])` |
| `cfg.boot_test("k").profile("test")` | `q.add(BootTest::new("k", &test))` |
| `cfg.include_kconfig(p)?` | `q.include_kconfig(p)`, without `?` |
| `cfg.rule(..)`, `cfg.bootloader(..)` | Removed. Use a `BuildStep` for a custom step |

## Example

```rust,ignore
// Before
pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("my-kernel", "0.1.0");
    let target = cfg.target("x86_64-unknown-none");
    cfg.cargo_workspace(".");
    cfg.profile("dev").runner("qemu").target(target)
        .opt_level(0).debug_info(true).run_binary("kernel");
    cfg.group("kernel").target(target).package("kernel")
        .linker_script("crates/kernel/kernel.ld");
    cfg.runner("qemu").qemu("qemu-system-x86_64").machine("q35").serial_stdio();
    Ok(())
}
```

```rust
// After
use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("my-kernel", "0.1.0");
    q.cargo_workspace(".");
    q.default_target(&Target::builtin("x86_64-unknown-none"));
    let kernel = q.add(Package::new("kernel").linker_script("crates/kernel/kernel.ld"));
    let qemu = q.add(Qemu::new("qemu-system-x86_64").machine("q35").serial(Serial::Stdio));
    q.run(&kernel).runner(&qemu);
    Ok(())
}
```

## Behaviour to check after porting

- A project whose crates use several non-host targets needs `q.run`
  or `q.default_target` to choose the one profiles build for.
  Otherwise the script fails with an error that lists the targets.
- A profile that inherits takes its target, run binary and runner from
  the root of its chain.
- `release` is now built in: inherits `dev`, opt-level 3, no debug
  info. Declare `Profile::release()` with changes, or a profile named
  `release`, to replace it.
