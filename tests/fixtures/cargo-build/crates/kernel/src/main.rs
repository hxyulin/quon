#![no_std]
#![no_main]

use fixture_derive::Marker;

#[allow(dead_code)]
#[derive(Marker)]
struct Kernel;

/// Cargo sets this for every compiled crate; Gluon must too.
#[used]
static VERSION: &str = env!("CARGO_PKG_VERSION");

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let _ = kernel::answer() + MARKER;
    loop {}
}
