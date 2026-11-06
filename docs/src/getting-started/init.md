# Generating quon.rs with quon init

`quon init` writes a `quon.rs` for a Cargo project you already have.
Run it in the directory that holds the root `Cargo.toml`: the workspace
root, or a package with no `[workspace]` table, which Quon treats as a
workspace of one, as Cargo does.

```sh
quon init                                  # build config only
quon init --bootloader limine --limine 11.4.1
quon init --dry-run                        # print instead of writing
```

It does not create a project in an empty directory, and it never
prompts. It refuses to replace an existing `quon.rs` unless you pass
`--force`.

## Host projects

When no target is named anywhere (no `--target`, no `build.target`, no
`targets` in `rust-toolchain.toml`) and nothing looks like a kernel,
`quon init` writes a host build. A member with a bin target looks
like a kernel when a `*.ld` file sits next to its `Cargo.toml` or its
bin source has `#![no_main]` or `#![no_std]`. `--kernel` and
`--bootloader` also ask for the kernel layout.

The host build declares every non-proc-macro member as a `Package`
for the host, and lists the same members in `q.host_tests()`:

```sh
quon build              # build every member for the host
quon run -- <args>      # build and run the bin
quon test               # unit tests, tests/ and doctests
```

`q.run` names the only bin target. With several, it is a
`// TODO:`.

The project has the two profiles every Quon project has, as Cargo
does: `dev`, the default, and `release`, which inherits from `dev` at
opt-level 3 without debug info. `quon build -r` builds with `release`.
rustc turns debug assertions and overflow checks off above opt-level
0, so `release` matches Cargo's release profile there too.

## What it reads for a kernel

| Setting | Source |
|---|---|
| Project name | The workspace directory's name |
| Project version | `workspace.package.version`, else the root package's `version`, else `0.1.0` |
| Target | `--target`, else `build.target` in `.cargo/config.toml`, else `targets` in `rust-toolchain.toml` |
| Kernel package | `--kernel`, else the only workspace member with a bin target (`[[bin]]` or `src/main.rs`) |
| `q.run` | The kernel package's bin target |
| Linker script | A `-Clink-arg=-T<path>` rustflag in `.cargo/config.toml`, else the only `*.ld` file next to the kernel's `Cargo.toml` |
| Kernel path on the ESP | The `path: boot():/...` line in `limine.conf` (or `KERNEL_PATH=` in `limine.cfg`), else `boot/kernel` |

Every other non-proc-macro member is declared as a `Package` too, so
`quon build` and `quon clippy` cover the whole workspace. Lints,
editions and target-specific dependencies stay in the Cargo manifests,
where Quon reads them on every build.

When the target list names several targets, Quon builds for one of them.
With `--target`, that is the first one given. Otherwise `quon init`
picks the first non-UEFI target, or the first `*-uefi` target with
`--bootloader uefi`. The others appear in a comment.

## What it leaves to you

What the files do not say becomes a `// TODO:` comment with the line to
fill in. The file builds before you edit it:

- packages that build only for another target (move them out of
  `LIBRARIES`);
- the linker script, when there is none or several;
- rustflags from `.cargo/config.toml`, which Quon does not read;
- the kernel, when no member or several members have a bin target;
- the QEMU `isa-debug-exit` device and `success_exit_code`;
- the Limine release, when `--limine` is not given, and `limine.conf`,
  when the project root has none.

With `--bootloader limine` and the target `x86_64-unknown-none`, the
kernel package also gets `-Crelocation-model=static`. That target links a
position-independent executable by default, and Limine loads one only
with a `PT_DYNAMIC` segment.

## Flags

| Flag | Description |
|---|---|
| `--target <TRIPLE>` | Target triple or `.json` spec. Repeat it for more targets; the first builds the workspace. |
| `--kernel <PACKAGE>` | The workspace member that holds the kernel. |
| `--bootloader limine\|uefi\|none` | `limine`: Limine boots the kernel from an ESP over UEFI. `uefi`: the kernel is a UEFI application at `EFI/BOOT/BOOTX64.EFI`. `none` (default): build config only, with the QEMU section commented out. |
| `--limine <VERSION>` | Pin this Limine release. Implies `--bootloader limine`. |
| `--force` | Overwrite an existing `quon.rs`. |
| `--dry-run` | Print the file to stdout instead of writing it. |

## Pinning Limine

`--limine 11.4.1` writes the `Download::limine` line with an empty SHA-256:

```rust,ignore
let limine = q.add(Download::limine("11.4.1", "BOOTX64.EFI", ""));
```

The first `quon build` fetches the file, fails, and prints its hash.
Check where the file came from, then paste the hash into `quon.rs`.
See [Limine Kernel](../cookbook/limine.md#pinning-limine).

Without `--limine`, the `Download::limine` line and the ESP entry that
uses it are
commented out, so the build runs without network access.
