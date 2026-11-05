#![no_std]
#![no_main]

// The generated config crate: one constant per option in gluon.rs.
// Gluon passes it as `--extern configurable_kernel_config` because the
// `kernel` group sets `config(true)`.
use configurable_kernel_config::{DEBUG, LOG_ENABLED, LOG_LEVEL};

/// Effective log level: 0 when logging is off.
#[used]
static EFFECTIVE_LOG_LEVEL: u32 = if LOG_ENABLED { LOG_LEVEL } else { 0 };

#[used]
static DEBUG_BUILD: bool = DEBUG;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// Entry point named by `ENTRY(_start)` in kernel.ld.
#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {}
}
