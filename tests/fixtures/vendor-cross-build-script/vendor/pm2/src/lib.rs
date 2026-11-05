#[cfg(not(pm2_probe_ok))]
compile_error!("pm2's rustc probe failed");

pub const VALUE: u32 = 40;
pub const TARGET: &str = include_str!(concat!(env!("OUT_DIR"), "/target.txt"));
