# External Dependencies

Gluon is a host-side tool that orchestrates other tools — rustc,
QEMU, OVMF firmware, the `hadris-fat` library — and parses output
formats produced by some of them. Each of those external surfaces
binds gluon to assumptions about the surface's shape and version.
This document records those assumptions, where they live, and what
breaks if they shift.

When a new gluon version drops, every entry below is a candidate
recheck point. When something in the toolchain ecosystem changes
incompatibly, this document is the inventory of impact sites.

## rustc dep-info format

**Where:** `cache::depinfo` (parser).
**What we depend on:** the Make-fragment shape rustc produces with
`--emit=dep-info`.

Rustc writes a Makefile-style fragment whose first target-line lists
the output followed by every source file it compiled. Gluon passes
`-Zbinary-dep-depinfo`, so the line also lists every rlib, rmeta and
proc-macro dylib rustc loaded:

```make
target/foo.rlib: src/lib.rs src/util.rs path\ with\ space.rs
src/lib.rs:
src/util.rs:
# env-dep:RUSTC_BOOTSTRAP=1
```

Gluon reads the **first target-line** and the `# env-dep:` comment
lines. The per-prereq stubs are ignored. An env-dep line is
`# env-dep:NAME=value`, or `# env-dep:NAME` when the variable was
unset. rustc escapes `\`, newline and carriage return in names and
values as `\\`, `\n` and `\r`; the parser undoes that escaping and
reads these lines before folding line continuations, so a value
ending in a backslash is not mistaken for one.

The parser handles three edge cases rustc actually produces:

1. **Backslash-escaped spaces in paths.** `foo\ bar.rs` decodes to
   `foo bar.rs` on tokenisation.
2. **Line continuations.** A trailing `\` immediately before a
   newline means the logical line continues on the next physical
   line. Joined with a single space before tokenisation.
3. **Leading blank / `#`-comment lines.** Rustc doesn't currently
   emit them at the top, but the Make-ish format allows them and we
   skip them defensively.

Anything more exotic (`$(VAR)` expansions, `;` recipes, etc.) is
outside the subset rustc emits. A malformed depfile is treated as a
toolchain bug, not a gluon bug — surfaced as `Error::Config` so the
diagnostic points at the file.

### Path encoding

Paths are emitted as raw bytes. On Unix, filesystem paths are
arbitrary byte sequences and may not be valid UTF-8: a project
under `/home/ülrich/…` would be silently corrupted by a
`read_to_string` + `b as char` pipeline. The parser therefore
operates on `&[u8]` and constructs each `PathBuf` via
`OsString::from_vec` on Unix and via UTF-8 validation on Windows
(where rustc writes depfiles in UTF-8 and the OS path encoding is
UTF-16, so a UTF-8 round-trip is the only sensible bridge).

### What breaks if rustc changes

If a future rustc version emits a meaningfully different format —
e.g. inline backslash sequences other than `\ `, or a non-Make
serialisation — `parse_depfile` will fail with `Error::Config` and
incremental compilation will halt with a clear diagnostic. The
parser does not silently fall through.

## QEMU CLI shape

**Where:** `run::qemu_cmd` (argv builder), `run::boot_mode` (per-mode
contributions), `run::ovmf` (firmware path resolution).
**What we depend on:** the QEMU command-line flag set, specifically
the `-machine`, `-m`, `-smp`, `-display`, `-serial`, `-kernel`,
`-drive`, `-device` flag families and their argument syntax.

The argv assembly is structured as a fixed token order, with
mode-specific wiring delegated to `BootModeAssembler` impls:

```
1. -machine <type>
2. -m <MB>M
3. -smp <cores>
4. -display <policy>          (skipped if user already set -display)
5. -serial <policy>           (skipped if user already set -serial)
6. <boot-mode contribution>   (DirectAssembler / UefiAssembler / BiosAssembler)
7. -device isa-debug-exit,... (test mode only)
8. <user extras from gluon.rs>
9. <CLI pass-through>
```

The token order is fixed but not checked at compile time; tests in
`run::qemu_cmd::tests` pin specific argvs as goldens. If QEMU
deprecates a flag or changes its argument syntax, those goldens
break first.

### Display policy

`-display` is only emitted if the user hasn't passed their own:

- `qemu().display(Default)` deliberately omits `-display` so QEMU
  picks its compiled-in backend (cocoa on macOS, gtk/sdl on Linux).
- `qemu().display(None)` emits `-display none` for headless / CI.
- `qemu().display(Spec("gtk"))` emits `-display gtk`.
- No `display(...)` call falls back to `-display none` — headless
  is the dominant CI case historically.

### isa-debug-exit (test mode)

`-device isa-debug-exit,iobase=0x{port:x},iosize=0x04` is wired in
ahead of user extras so `gluon test -- <extras>` doesn't clobber it
by accident. The `iosize=0x04` is fixed because QEMU's
`isa-debug-exit` device takes a 32-bit write at that offset; the
`port` is configurable per `qemu().test_exit_port(...)`. The
convention follows the Rust embedonomicon's bare-metal test exit
recipe.

### What breaks if QEMU changes

Goldens in `run::qemu_cmd::tests` will diff. Bumping QEMU on the
host doesn't break gluon as long as the flag set is preserved;
QEMU has historically been very conservative about removing flags.

## OVMF firmware paths

**Where:** `run::ovmf::SYSTEM_PATHS` (probe table).
**What we depend on:** distro-specific OVMF install locations.

Gluon does not ship OVMF. Users provide `OVMF_CODE.fd` and
`OVMF_VARS.fd` paths via, in precedence order:

1. Explicit `.ovmf_code(...)` / `.ovmf_vars(...)` calls in
   `gluon.rs` → `QemuDef::ovmf_code` / `ovmf_vars`.
2. `OVMF_CODE` / `OVMF_VARS` environment variables.
3. The firmware installed beside the QEMU binary that `gluon run` will
   spawn: `<bin>/../share/qemu/edk2-x86_64-code.fd` and
   `edk2-i386-vars.fd`. A bare binary name is searched on `PATH`, and
   symlinks are resolved first, so a Nix profile link leads to the
   store path that holds the firmware. Nix, Homebrew and QEMU source
   builds all install this pair.
4. A small table of well-known system paths per OS.

If none of those resolve, gluon emits `Error::OvmfNotFound` with a
multi-line diagnostic listing every fallback the resolver tried, so
the user knows exactly what to install or set.

### System path table

The probe table is ordered Linux (most common) → macOS → anything
more exotic. Earlier probes are hot paths.

| Distro / source | CODE path | VARS path |
|---|---|---|
| Ubuntu/Debian `ovmf` | `/usr/share/OVMF/OVMF_CODE.fd` | `/usr/share/OVMF/OVMF_VARS.fd` |
| Fedora/RHEL `edk2-ovmf` | `/usr/share/edk2-ovmf/x64/OVMF_CODE.fd` | `/usr/share/edk2-ovmf/x64/OVMF_VARS.fd` |
| Arch `edk2-ovmf` | `/usr/share/edk2/x64/OVMF_CODE.fd` | `/usr/share/edk2/x64/OVMF_VARS.fd` |
| Homebrew (Apple Silicon) `qemu` | `/opt/homebrew/share/qemu/edk2-x86_64-code.fd` | `/opt/homebrew/share/qemu/edk2-i386-vars.fd` |
| Homebrew (Intel) `qemu` | `/usr/local/share/qemu/edk2-x86_64-code.fd` | `/usr/local/share/qemu/edk2-i386-vars.fd` |

**Adding a new entry:** add at the bottom of `SYSTEM_PATHS`. Existing
entries are hot paths.

The first match whose **both** files exist wins — a partial install
(e.g. only CODE present) does not satisfy a probe.

### Writable VARS copy

UEFI updates NVRAM on every boot, so QEMU requires `VARS` to be
writable. System paths typically install OVMF as `0444` for
non-root users. The resolver detects read-only sources and copies
to `<build_root>/ovmf_vars-<profile>.fd`, returning the writable
copy. Re-copy if the source mtime is newer than the cached copy.

The per-profile suffix exists so two concurrent runs of different
profiles don't race on the same NVRAM file — NVRAM state is
per-boot-environment and shouldn't leak between profiles.

### What breaks if a distro repackages

If Debian renames `/usr/share/OVMF/` to something else, `gluon run
--mode uefi` on Ubuntu/Debian will fall through to the next probe
or to the `OvmfNotFound` diagnostic. The diagnostic lists every
fallback tried, so the user can immediately see they need either
to install OVMF differently or to set `OVMF_CODE`/`OVMF_VARS`
explicitly.

Add new probe entries when new common distro paths emerge.

## hadris-fat (FAT32 image writer)

**Where:** `image::fat`.
**What we depend on:** the `hadris-fat` crate's
`FatVolumeFormatter`, `FatFs`, `FatFsWriteExt` API, and the FAT32
spec it implements.

### FAT32 minimum size

FAT32 has a per-spec minimum of roughly 33 MiB (the size needed for
a viable FAT). `compute_size_bytes` in `image::mod` floors at
**64 MiB** to give a safety margin. Going below this would produce
an image that QEMU and the firmware don't recognise as FAT32.

### Volume label collision

Hadris (like most FAT formatters) writes the volume label as a
special directory entry in the root directory with the `volume_id`
attribute set. Hadris's `find()` walks all root entries — including
the volume label entry — which would produce a "non-directory of
that name already exists" error if the user's image happened to be
named the same as a top-level directory in their `dest_path`s.

To sidestep this entirely, the volume label is hardcoded to
`"GLUON"`. Volume labels aren't load-bearing for boot — UEFI looks
at the FS type byte, not the label. A future
`image().volume_label("...")` builder can override this once the
collision-free listing API exists in hadris.

### Determinism

FAT directory ordering follows file-creation order. `image::fat`
writes entries in the order the user wrote them in `gluon.rs`
(`Vec<ImageEntry>` preserves insertion order), so the byte layout
is reproducible across runs assuming inputs match. Subdirectory
walks use `walkdir::WalkDir::new(...).sort_by_file_name()` to keep
that ordering stable across filesystems.

### Cache aux-hash

Image cache keys (see [cache architecture](./cache.md)) include the
hadris-fat crate version in the `aux_hash`, so library bumps
invalidate correctly even when the user's config and inputs haven't
changed. A version bump produces a one-time full image rebuild.

### What breaks if hadris-fat changes

A hadris-fat API change is a hard build error in `image::fat`. A
behavioural change (e.g. different on-disk byte layout for
identical inputs) is caught by the integration tests that hash
produced images.

## RUSTC_BOOTSTRAP and `-Z` flags

**Where:** `sysroot::ensure_sysroot` (and `compile/sysroot.rs`).
**What we depend on:** rustc's unstable `RUSTC_BOOTSTRAP=1`
environment switch and `-Z force-unstable-if-unmarked`.

`core`, `compiler_builtins`, and `alloc` use `#![feature(...)]`
internally, so a stable `rustc` cannot compile them without opting
in. Gluon sets `RUSTC_BOOTSTRAP=1` in the child environment for
every sysroot rustc invocation — the same switch Cargo's
`build-std` uses — and passes `-Z force-unstable-if-unmarked` so
downstream crates cannot accidentally depend on internals of the
sysroot crates as if they were stable.

Both are standard for custom-sysroot flows. If the rust compiler
ever stabilises sysroot building (unlikely soon), these can be
removed.

## Summary table

| Surface | Where | Breakage signal |
|---|---|---|
| rustc dep-info format | `cache::depinfo` | `Error::Config` parse failure |
| rustc unstable bootstrap | `sysroot` | sysroot build fails |
| QEMU flags | `run::qemu_cmd`, `run::boot_mode` | argv golden tests diff |
| OVMF system paths | `run::ovmf::SYSTEM_PATHS` | `OvmfNotFound` diagnostic |
| hadris-fat API | `image::fat` | compile error or image-hash test diff |
| FAT32 size minimum | `image::mod::compute_size_bytes` | image refuses to mount |
