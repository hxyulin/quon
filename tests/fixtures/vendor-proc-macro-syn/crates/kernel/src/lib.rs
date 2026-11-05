#![no_std]

#[derive(Clone, Copy, bytemuck_derive::Pod, bytemuck_derive::Zeroable)]
#[repr(C)]
pub struct Pair {
    pub a: u32,
    pub b: u32,
}

pub fn first(bytes: &[u8; 8]) -> u32 {
    bytemuck::cast::<[u8; 8], Pair>(*bytes).a
}
