#![no_std]
#![no_main]

// Any `fakeflags` works here, so the network test can swap in a real
// crates.io package under the same name.
use fakeflags as _;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {}
}
