#![no_std]

pub trait Hello {
    fn hello() -> u32;
}

#[derive(hello_derive::Hello)]
pub struct K;

pub fn total() -> u32 {
    K::hello() + consumer::OK + consumer::probe_sys::GEN + consumer::probe_sys::FLAVOR
}
