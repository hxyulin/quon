#![no_std]

include!(concat!(env!("OUT_DIR"), "/gen.rs"));

pub const GREETING: &str = env!("PROBE_GREETING");

#[cfg(not(probe_new))]
compile_error!("the build script's `rustc-cfg` did not arrive");

#[cfg(not(feature = "fancy"))]
compile_error!("`consumer` enables `fancy`");
