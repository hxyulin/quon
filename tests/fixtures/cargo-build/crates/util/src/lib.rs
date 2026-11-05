#![no_std]

pub fn base() -> u32 {
    40
}

#[cfg(feature = "extra")]
pub fn extra() -> u32 {
    1
}

#[cfg(not(feature = "extra"))]
pub fn extra() -> u32 {
    0
}
