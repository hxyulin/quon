//! Bare-metal kernel that the bootloader embeds with `include_bytes!`.
//! It never runs in this example; the bootloader halts before handing
//! off.

#![no_std]
#![no_main]

/// Kept in the image so the bootloader can find it.
#[used]
#[no_mangle]
pub static KERNEL_MAGIC: [u8; 8] = boot_info::KERNEL_MAGIC;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// Entry point named by `ENTRY(_start)` in kernel.ld.
#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {}
}
