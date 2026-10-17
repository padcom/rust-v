# Lazy vector-unit enabling, as Linux does it: each iteration turns the vector unit off, so the
# VSETVLI traps, the handler turns the unit on and returns to it. In compiled code the
# VSETVLI must raise the trap too (not hand back to the dispatcher, which retries forever).
.option norvc
_start:
    la t0, handler
    csrw mtvec, t0
    li t3, (3 << 9)          # mstatus.VS
    li s2, 0                 # traps taken
    li t0, 100000
loop:
    csrc mstatus, t3         # vector unit off (ends the block)
    vsetvli t1, t0, e8, m1, ta, ma
    addi t0, t0, -1
    bnez t0, loop
    li t1, 0x5555
    li t2, 100000
    beq s2, t2, 1f
    li t1, (5 << 16) | 0x3333
1:  li t2, 0x100000
    sw t1, 0(t2)
2:  j 2b

handler:
    csrr t4, mcause
    li t5, 2                 # illegal instruction
    bne t4, t5, fail
    csrs mstatus, t3
    addi s2, s2, 1
    mret
fail:
    li t1, (7 << 16) | 0x3333
    li t2, 0x100000
    sw t1, 0(t2)
3:  j 3b
