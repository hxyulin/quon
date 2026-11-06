# Installation

## Requirements

- **Rust 1.99 or later.** Stable works, except for custom target-spec
  `.json` files, which need nightly.
- **The `rust-src` component**, for cross targets. Gluon compiles their
  sysroots (`core`, `alloc`, `compiler_builtins`) from source. Host builds
  do not need it.
- **Optional Rust components:** `clippy` (for `gluon clippy`), `rustfmt` (for
  `gluon fmt`), and `rust-analyzer` (for IDE integration via
  `gluon configure`).
- **QEMU** -- only needed if you want to use `gluon run`. Not required for
  building alone.
- **OVMF firmware** -- only needed for UEFI boot. If you are targeting BIOS or
  a non-UEFI boot protocol, you can skip this.

## Install Gluon

Clone the repository and install the CLI:

```sh
git clone https://github.com/hxyulin/gluon.git
cd gluon
cargo install --path crates/gluon-cli
```

The repository's `rust-toolchain.toml` pins the nightly toolchain used
to develop Gluon, so `cargo install` inside the clone uses it. Gluon
itself builds on stable too.

The installed `gluon` carries the sources that `gluon.rs` scripts link
against, so you can delete or move the clone afterwards. Installing
straight from Git works too:

```sh
cargo install --git https://github.com/hxyulin/gluon.git gluon-cli
```

Verify the installation:

```sh
gluon --version
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
