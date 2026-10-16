    .section .text.init
    .globl _start
_start:
    la sp, __stack_top
    la t0, __bss_start
    la t1, __bss_end
1:  bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b
2:  call main
    /* Power off through the SiFive test device. */
    li t0, 0x100000
    li t1, 0x5555
    sw t1, 0(t0)
3:  j 3b
