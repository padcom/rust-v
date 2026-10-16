# Cross-modifying code: hart 1 rewrites a function hart 0 already executed, then hart 0
# executes FENCE.I and must see the new code.
.option norvc
_start:
    li s0, 0x80003000        # flags: A at 0, B at 64, C at 128
    li s1, 0x80001000        # func
    beqz a0, hart0
    li t0, 1
    bne a0, t0, park
hart1:
    sw zero, 0x7f0(s1)       # write the (not yet code) page: leaves a store-TLB entry
    li t0, 1
    sw t0, 0(s0)             # A = 1
1:  lw t0, 64(s0)            # wait for B
    beqz t0, 1b
    li t0, 0x00200513        # li a0, 2
    sw t0, 0(s1)             # rewrite func through the old TLB entry
    fence w, w
    li t0, 1
    sw t0, 128(s0)           # C = 1
    j park
hart0:
2:  lw t0, 0(s0)             # wait for A
    beqz t0, 2b
    jalr ra, 0(s1)           # func: decoded and cached here
    li t0, 1
    bne a0, t0, fail
    sw t0, 64(s0)            # B = 1
3:  lw t0, 128(s0)           # wait for C
    beqz t0, 3b
    fence.i
    jalr ra, 0(s1)
    li t0, 2
    bne a0, t0, fail
    li t1, 0x5555
    j finish
fail:
    li t1, (4 << 16) | 0x3333
finish:
    li t2, 0x100000
    sw t1, 0(t2)
park:
    wfi
    j park
.org 0x1000
func:
    li a0, 1
    ret
