#![no_std]

pub use probe_sys;

#[cfg(consumer_ok)]
pub const OK: u32 = 1;
