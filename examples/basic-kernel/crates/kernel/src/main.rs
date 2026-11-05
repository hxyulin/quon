#![no_std]
#![no_main]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// Entry point named by `ENTRY(_start)` in kernel.ld.
#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {}
}
