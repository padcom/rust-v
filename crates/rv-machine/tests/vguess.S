# A function without its own VSETVLI is called under alternating vector configurations, so
# its compiled code (specialised for whichever configuration it was compiled under) must
# check the configuration and fall back when it differs.
.option norvc
_start:
    li t0, (1 << 9)          # mstatus.VS = Initial
    csrs mstatus, t0
    vsetvli t1, zero, e8, m1, ta, ma
    vmv.v.i v1, 1            # bytes 0x01
    vmv.v.i v2, -1           # bytes 0xff
    li s0, 20000
loop:
    andi t0, s0, 1
    beqz t0, 1f
    vsetvli t1, zero, e8, m1, ta, ma
    j 2f
1:  vsetvli t1, zero, e32, m1, ta, ma
2:  call add
    addi s0, s0, -1
    bnez s0, loop
    # e32: 0x01010101 + 0xffffffff = 0x01010100; e8: 0x01 + 0xff = 0x00 per byte.
    vsetvli t1, zero, e32, m1, ta, ma
    call add
    vmv.x.s a0, v3
    li t2, 0x01010100
    bne a0, t2, fail
    vsetvli t1, zero, e8, m1, ta, ma
    call add
    vsetvli t1, zero, e32, m1, ta, ma
    vmv.x.s a0, v3
    bnez a0, fail
    li t1, 0x5555
    j done
fail:
    li t1, (3 << 16) | 0x3333
done:
    li t2, 0x100000
    sw t1, 0(t2)
3:  j 3b

.balign 64
add:
    vadd.vv v3, v1, v2
    ret
