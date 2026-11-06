//! Minimal UEFI bootloader: a raw `efi_main` (no `uefi` crate) for
//! `x86_64-unknown-uefi`, whose builtin target spec makes `efi_main`
//! the entry symbol.
//!
//! It returns `EFI_LOAD_ERROR` if the kernel image it embeds is empty,
//! else halts. There is no handoff: the example shows the build, not an
//! OS.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// The compiled kernel. Quon sets `KERNEL_PATH` for this crate's rustc
/// because quon.rs declares `artifact_env("KERNEL_PATH", "kernel")`.
static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));

/// The magic the kernel image carries, for a debugger or a later
/// handoff to look for.
#[used]
static EXPECTED_MAGIC: [u8; 8] = boot_info::KERNEL_MAGIC;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    halt()
}

/// UEFI entry point. The two parameters are `EFI_HANDLE` and
/// `EFI_SYSTEM_TABLE*`, unused here.
#[no_mangle]
pub extern "efiapi" fn efi_main(
    _image_handle: *mut core::ffi::c_void,
    _system_table: *mut core::ffi::c_void,
) -> usize {
    const EFI_LOAD_ERROR: usize = (1 << 63) | 1;
    if KERNEL.is_empty() {
        return EFI_LOAD_ERROR;
    }
    halt()
}

fn halt() -> ! {
    loop {
        // SAFETY: `hlt` touches no memory and only pauses the CPU.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
