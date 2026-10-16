ENTRY(_start)
SECTIONS
{
    . = 0x80000000;
    .text : { *(.text.init) *(.text .text.*) }
    .rodata : { *(.rodata .rodata.* .srodata .srodata.*) }
    .data : { *(.data .data.* .sdata .sdata.*) }
    . = ALIGN(8);
    __bss_start = .;
    .bss : { *(.bss .bss.* .sbss .sbss.* COMMON) }
    . = ALIGN(8);
    __bss_end = .;
    . = ALIGN(16) + 0x10000;
    __stack_top = .;
}
