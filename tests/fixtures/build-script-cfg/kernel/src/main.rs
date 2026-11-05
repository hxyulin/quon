#![no_std]
#![no_main]

#[used]
static GREETING: &str = consumer::GREETING;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {}
}
