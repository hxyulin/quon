//! Data the bootloader and the kernel agree on.

#![no_std]

/// Bytes the kernel embeds and the bootloader looks for in the kernel
/// image it carries.
pub const KERNEL_MAGIC: [u8; 8] = *b"GLUONKER";
