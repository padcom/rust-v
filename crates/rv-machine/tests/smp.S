# Runs on every hart from reset (a0 = hartid, M-mode, no firmware).
.option norvc
.equ N, 20000
_start:
    li s0, 0x80001000        # shared area
    # Phase 1: AMO counter at 0(s0).
    li t0, N
    li t1, 1
1:  amoadd.w zero, t1, (s0)
    addi t0, t0, -1
    bnez t0, 1b
    # Phase 2: LR/SC counter at 64(s0).
    addi s1, s0, 64
    li t0, N
2:  lr.w t1, (s1)
    addi t1, t1, 1
    sc.w t2, t1, (s1)
    bnez t2, 2b
    addi t0, t0, -1
    bnez t0, 2b
    # Phase 3: message passing between hart 1 (writer) and hart 0 (reader).
    addi s2, s0, 128         # data
    addi s3, s0, 192         # flag
    addi s4, s0, 256         # ack
    li t0, 1
    li t3, N
    beqz a0, reader
    li t4, 1
    bne a0, t4, barrier
writer:
    sw t0, 0(s2)
    fence w, w
    sw t0, 0(s3)
3:  lw t1, 0(s4)
    bne t1, t0, 3b
    addi t0, t0, 1
    ble t0, t3, writer
    j barrier
reader:
4:  lw t1, 0(s3)
    bne t1, t0, 4b
    fence r, r
    lw t2, 0(s2)
    bne t2, t0, fail_mp
    sw t0, 0(s4)
    addi t0, t0, 1
    ble t0, t3, reader
barrier:
    addi s5, s0, 320
    li t1, 1
    amoadd.w zero, t1, (s5)
    bnez a0, park
5:  lw t1, 0(s5)
    li t2, 4
    bne t1, t2, 5b
    # Verdict.
    li t2, 4 * N
    lw t1, 0(s0)
    bne t1, t2, fail_amo
    lw t1, 64(s0)
    bne t1, t2, fail_lrsc
    li t1, 0x5555
    j finish
fail_amo:
    li t1, (1 << 16) | 0x3333
    j finish
fail_lrsc:
    li t1, (2 << 16) | 0x3333
    j finish
fail_mp:
    li t1, (3 << 16) | 0x3333
finish:
    li t2, 0x100000
    sw t1, 0(t2)
park:
    wfi
    j park
