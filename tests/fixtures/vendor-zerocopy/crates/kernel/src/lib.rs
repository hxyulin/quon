#![no_std]

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub len: u32,
}

pub fn parse(bytes: &[u8]) -> Option<&Header> {
    Header::ref_from_prefix(bytes).ok().map(|(h, _)| h)
}

pub fn magic_bytes(h: &Header) -> &[u8] {
    h.magic.as_bytes()
}
