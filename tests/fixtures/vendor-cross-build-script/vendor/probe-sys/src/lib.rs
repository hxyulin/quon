#![no_std]

include!(concat!(env!("OUT_DIR"), "/gen.rs"));

#[cfg(not(probe_core_ok))]
compile_error!("probe-sys's script could not compile for the target");

#[cfg(probe_new)]
pub const GREETING: &str = env!("PROBE_GREETING");

#[cfg(probe_flavor = "plain")]
pub const FLAVOR: u32 = 1;
#[cfg(not(probe_flavor = "plain"))]
pub const FLAVOR: u32 = 2;
