#[cfg(not(qt_built))]
compile_error!("qt's build script output is missing");

pub use pm2;

pub const VALUE: u32 = 2;
