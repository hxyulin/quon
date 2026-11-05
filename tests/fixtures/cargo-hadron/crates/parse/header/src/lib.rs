//! A fixed-layout header read with zerocopy.

#![no_std]

use hello_derive::Hello;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// An 8-byte header: magic, then payload length.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Hello)]
#[repr(C)]
pub struct Header {
    /// Must be [`MAGIC`].
    pub magic: u32,
    /// Payload length in bytes.
    pub len: u32,
}

/// The expected [`Header::magic`].
pub const MAGIC: u32 = 0x4844_5221;

/// The header at the start of `bytes`, if it is long enough and has
/// the right magic.
#[must_use]
pub fn parse(bytes: &[u8]) -> Option<&Header> {
    let (header, _) = Header::ref_from_prefix(bytes).ok()?;
    (header.magic == MAGIC).then_some(header)
}

/// `Header::HELLO` comes from the derive.
pub const NAME: &str = Header::HELLO;
