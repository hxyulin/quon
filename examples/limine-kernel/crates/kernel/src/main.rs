//! Minimal Limine-protocol kernel: reports which bootloader started
//! it on the first serial port, then exits QEMU through the
//! `isa-debug-exit` device.

#![no_std]
#![no_main]

use core::arch::asm;
use core::fmt::{self, Write};

use limine::request::BootloaderInfoRequest;
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

#[used]
#[unsafe(link_section = ".limine_requests_start")]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

/// I/O port of the first serial port (COM1).
const COM1: u16 = 0x3f8;
/// I/O port of QEMU's `isa-debug-exit` device, as set in quon.rs.
const DEBUG_EXIT: u16 = 0xf4;
/// Value written to `DEBUG_EXIT` on success. QEMU exits with status
/// `(value << 1) | 1`, so 0x10 becomes 33.
const EXIT_SUCCESS: u32 = 0x10;
const EXIT_FAILURE: u32 = 0x11;

/// Writes to COM1. QEMU's emulated UART needs no setup to transmit.
struct Serial;

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // SAFETY: COM1's data register accepts any byte; writing it
            // has no effect on memory.
            unsafe { asm!("out dx, al", in("dx") COM1, in("al") byte) };
        }
        Ok(())
    }
}

fn exit_qemu(code: u32) -> ! {
    // SAFETY: the `isa-debug-exit` device ends the QEMU process; on
    // hardware without it the write is ignored and we halt below.
    unsafe { asm!("out dx, eax", in("dx") DEBUG_EXIT, in("eax") code) };
    loop {
        // SAFETY: `hlt` only stops the CPU until the next interrupt.
        unsafe { asm!("hlt") };
    }
}

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let mut serial = Serial;
    match BOOTLOADER_INFO.response() {
        Some(info) => {
            let _ = writeln!(
                serial,
                "limine-kernel: booted by {} {} (base revision {:?}, requested {} supported: {})",
                info.name(),
                info.version(),
                BASE_REVISION.actual_revision(),
                BaseRevision::MAX_SUPPORTED,
                BASE_REVISION.is_supported(),
            );
            exit_qemu(EXIT_SUCCESS)
        }
        None => {
            let _ = writeln!(serial, "limine-kernel: no bootloader info response");
            exit_qemu(EXIT_FAILURE)
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = writeln!(Serial, "limine-kernel: {info}");
    exit_qemu(EXIT_FAILURE)
}
