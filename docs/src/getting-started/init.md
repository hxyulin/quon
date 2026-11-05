# Generating gluon.rs with gluon init

`gluon init` writes a `gluon.rs` for a Cargo workspace you already have.
Run it in the directory that holds the workspace `Cargo.toml`:

```sh
gluon init                                  # build config only
gluon init --bootloader limine --limine 11.4.1
gluon init --dry-run                        # print instead of writing
```

It does not create a kernel in an empty directory, and it never prompts.
It refuses to replace an existing `gluon.rs` unless you pass `--force`.

## What it reads

| Setting | Source |
|---|---|
| Project name | The workspace directory's name |
| Project version | `workspace.package.version`, else the root package's `version`, else `0.1.0` |
| Target | `--target`, else `build.target` in `.cargo/config.toml`, else `targets` in `rust-toolchain.toml` |
| Kernel package | `--kernel`, else the only workspace member with a bin target (`[[bin]]` or `src/main.rs`) |
| `boot_binary` | The kernel package's bin target |
| Linker script | A `-Clink-arg=-T<path>` rustflag in `.cargo/config.toml`, else the only `*.ld` file next to the kernel's `Cargo.toml` |
| Kernel path on the ESP | The `path: boot():/...` line in `limine.conf` (or `KERNEL_PATH=` in `limine.cfg`), else `boot/kernel` |

Every other non-proc-macro member goes into a `libraries` group, so
`gluon build` and `gluon clippy` cover the whole workspace. Lints,
editions and target-specific dependencies stay in the Cargo manifests,
where Gluon reads them on every build.

When the target list names several targets, Gluon builds for one of them.
With `--target`, that is the first one given. Otherwise `gluon init`
picks the first non-UEFI target, or the first `*-uefi` target with
`--bootloader uefi`. The others appear in a comment.

## What it leaves to you

What the files do not say becomes a `// TODO:` comment with the line to
fill in. The file builds before you edit it:

- packages that build only for another target (move them out of
  `LIBRARIES`);
- the linker script, when there is none or several;
- rustflags from `.cargo/config.toml`, which Gluon does not read;
- the kernel, when no member or several members have a bin target;
- the QEMU `isa-debug-exit` device and `success_exit_code`;
- the Limine release, when `--limine` is not given, and `limine.conf`,
  when the project root has none.

With `--bootloader limine` and the target `x86_64-unknown-none`, the
kernel group also gets `-Crelocation-model=static`. That target links a
position-independent executable by default, and Limine loads one only
with a `PT_DYNAMIC` segment.

## Flags

| Flag | Description |
|---|---|
| `--target <TRIPLE>` | Target triple or `.json` spec. Repeat it for more targets; the first builds the workspace. |
| `--kernel <PACKAGE>` | The workspace member that holds the kernel. |
| `--bootloader limine\|uefi\|none` | `limine`: Limine boots the kernel from an ESP over UEFI. `uefi`: the kernel is a UEFI application at `EFI/BOOT/BOOTX64.EFI`. `none` (default): build config only, with the QEMU section commented out. |
| `--limine <VERSION>` | Pin this Limine release. Implies `--bootloader limine`. |
| `--force` | Overwrite an existing `gluon.rs`. |
| `--dry-run` | Print the file to stdout instead of writing it. |

## Pinning Limine

`--limine 11.4.1` writes the `cfg.limine` line with an empty SHA-256:

```rust
cfg.limine("11.4.1").file("BOOTX64.EFI", "");
```

The first `gluon build` fetches the file, fails, and prints its hash.
Check where the file came from, then paste the hash into `gluon.rs`.
See [Limine Kernel](../cookbook/limine.md#pinning-limine).

Without `--limine`, the `cfg.limine` and `add_download` lines are
commented out, so the build runs without network access.
