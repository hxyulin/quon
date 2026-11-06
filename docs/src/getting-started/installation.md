# Installation

## Requirements

- **Rust 1.99 or later.** Stable works, except for custom target-spec
  `.json` files, which need nightly.
- **Each builtin target you build for**, installed with
  `rustup target add <triple>`. Quon links its prebuilt `core`,
  `alloc` and `compiler_builtins`.
- **The `rust-src` component**, for custom JSON targets and targets
  declared with `.build_std()`. Quon compiles their sysroots from
  source.
- **Optional Rust components:** `clippy` (for `quon clippy`), `rustfmt` (for
  `quon fmt`), and `rust-analyzer` (for IDE integration via
  `quon configure`).
- **QEMU** -- only needed if you want to use `quon run`. Not required for
  building alone.
- **OVMF firmware** -- only needed for UEFI boot. If you are targeting BIOS or
  a non-UEFI boot protocol, you can skip this.

## Install Quon

Clone the repository and install the CLI:

```sh
git clone https://github.com/hxyulin/quon.git
cd quon
cargo install --path crates/quon
```

The repository's `rust-toolchain.toml` pins the nightly toolchain used
to develop Quon, so `cargo install` inside the clone uses it. Quon
itself builds on stable too.

The installed `quon` carries the sources that `quon.rs` scripts link
against, so you can delete or move the clone afterwards. Installing
straight from Git works too:

```sh
cargo install --git https://github.com/hxyulin/quon.git quon
```

Verify the installation:

```sh
quon --version
```

## Adding rust-src

If your toolchain does not already include `rust-src`, add it manually:

```sh
rustup component add rust-src
```

You can confirm it is present with:

```sh
rustup component list --installed | grep rust-src
```

## Optional: QEMU and OVMF

Install QEMU through your system package manager:

```sh
# macOS
brew install qemu

# Ubuntu / Debian
sudo apt install qemu-system-x86 qemu-system-aarch64

# Arch Linux
sudo pacman -S qemu-full
```

For UEFI boot, you will also need OVMF firmware. On most Linux distributions
it is available as the `ovmf` or `edk2-ovmf` package. On macOS, OVMF can be
obtained via Homebrew or built from source.
