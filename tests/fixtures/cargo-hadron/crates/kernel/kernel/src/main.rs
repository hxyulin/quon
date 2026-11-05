//! A kernel that parses a header at boot.

#![no_std]
#![no_main]

static IMAGE: [u8; 8] = [0x21, 0x52, 0x44, 0x48, 4, 0, 0, 0];

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// Entry point.
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let ok = header::parse(&IMAGE).is_some_and(|h| h.len == 4) && !header::NAME.is_empty();
    core::hint::black_box(ok);
    loop {}
}
