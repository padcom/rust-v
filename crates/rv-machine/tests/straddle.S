# The loop branch straddles a page boundary, so the loop body block falls through to it and
# the interpreter runs it on its own. A chain link from the body must not skip the branch
# (which would loop forever, since the counter is never checked).
.option norvc
_start:
    li t0, 1000
    li a0, 0
    j loop
.org 0xff6
loop:
    addi a0, a0, 1          # 0xff6
    addi t0, t0, -1         # 0xffa: the block ends after this
    bnez t0, loop           # 0xffe..0x1001: straddles the page boundary
    li t1, 0x5555
    li t2, 1000
    beq a0, t2, 1f
    li t1, (5 << 16) | 0x3333
1:  li t2, 0x100000
    sw t1, 0(t2)
2:  j 2b
