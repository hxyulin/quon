/* INTENTIONALLY WRONG: OUTPUT_FORMAT(binary) tells lld to skip ELF
   wrapping. After Task 6, quon controls output format itself; this
   directive collides with the new contract and causes objcopy to fail
   the magic-byte pre-check on the supposed `.elf`. The integration
   test asserts the resulting diagnostic mentions OUTPUT_FORMAT(binary)
   so the user can fix their linker script. */
OUTPUT_FORMAT(binary)

ENTRY(_start)

SECTIONS
{
    . = 0x7c00;
    .text : { *(.text*) }
    .rodata : { *(.rodata*) }
    .data : { *(.data*) }
    .bss : { *(.bss*) }
}
