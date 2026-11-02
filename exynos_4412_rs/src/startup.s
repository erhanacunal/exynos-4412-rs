@ Reset path, vector table and exception stubs. Runs in ARM state.
@
@ Included with `global_asm!(..., options(raw))`, so braces are plain assembly.

.syntax unified
.arm
.fpu vfpv3

.equ MODE_FIQ, 0x11
.equ MODE_IRQ, 0x12
.equ MODE_SVC, 0x13
.equ MODE_ABT, 0x17
.equ MODE_UND, 0x1b

@ Keep in sync with the ASSERT in link.x. 0x40 is also the size of a uImage header.
.equ RELOCATE_MIN_OFFSET, 0x40

.equ SCTLR_M, 1 << 0
.equ SCTLR_A, 1 << 1
.equ SCTLR_C, 1 << 2
.equ SCTLR_Z, 1 << 11
.equ SCTLR_I, 1 << 12
.equ SCTLR_V, 1 << 13
.equ SCTLR_TRE, 1 << 28
.equ SCTLR_AFE, 1 << 29
.equ SCTLR_TE, 1 << 30

@ Linux 5.15.198 arch/arm/mm/proc-v7.S:306 "Enable SMP/nAMP mode" and
@ __v7_ca9mp_setup "Cache/TLB ops broadcasting".
.equ ACTLR_SMP, 1 << 6
.equ ACTLR_FW, 1 << 0

@ arch/arm/include/asm/domain.h:48,56, same value as mmu.rs.
.equ DACR_DOMAIN0_CLIENT, 1

@ UART2 UTXH: soc.rs UART2_BASE + 0x20 (drivers/uart.rs), for the bring-up markers below.
.equ MARKER_UTXH, 0x13820020

.equ CPACR_CP10_CP11_FULL, 0xf << 20
.equ CPACR_ASEDIS_D32DIS, 0x3 << 30
.equ FPEXC_EN, 1 << 30

@ MMU-off copy of the common CPU state setup.
.macro QUIESCE_CPU
    @ SCTLR.V=0 makes the CPU honour VBAR, TE=0 takes exceptions in ARM state.
    @ SCTLR.A=1 because unaligned accesses are UNPREDICTABLE on the
    @ Strongly-ordered memory an MMU-off core sees; now they fault precisely.
    mrc     p15, 0, r0, c1, c0, 0           @ SCTLR
    bic     r0, r0, #(SCTLR_M | SCTLR_C)
    bic     r0, r0, #(SCTLR_Z | SCTLR_I | SCTLR_V)
    bic     r0, r0, #SCTLR_TE
    orr     r0, r0, #SCTLR_A
    mcr     p15, 0, r0, c1, c0, 0
    isb
    mov     r0, #0
    mcr     p15, 0, r0, c7, c5, 0           @ ICIALLU
    mcr     p15, 0, r0, c7, c5, 6           @ BPIALL
    mcr     p15, 0, r0, c8, c7, 0           @ TLBIALL
    dsb
    isb
.endm

@ VFP/NEON on before any Rust runs: the hard-float ABI may use it anywhere,
@ including d16-d31, so ASEDIS and D32DIS must end up clear too.
@ Reading CPACR back catches a Secure world whose NSACR denies access (the
@ board has a secure firmware node); touching FPEXC then would trap, and the
@ UND stub itself needs the VFP. Branches to \fail on failure.
.macro ENABLE_VFP fail
    mrc     p15, 0, r0, c1, c0, 2           @ CPACR
    orr     r0, r0, #CPACR_CP10_CP11_FULL
    bic     r0, r0, #CPACR_ASEDIS_D32DIS
    mcr     p15, 0, r0, c1, c0, 2
    isb
    mrc     p15, 0, r0, c1, c0, 2
    tst     r0, #CPACR_ASEDIS_D32DIS
    bne     \fail
    and     r0, r0, #CPACR_CP10_CP11_FULL
    cmp     r0, #CPACR_CP10_CP11_FULL
    bne     \fail
    mov     r0, #FPEXC_EN
    vmsr    fpexc, r0
    mov     r0, #0
    vmsr    fpscr, r0
.endm

@ Last words of a secondary core that has to halt: its number, then \letter, straight to
@ the UART data register. Nothing else works before the core is coherent, and it needs no
@ lock because CPU0 stays silent until the core reports online or times out.
@ Needs the CPU number in r10; clobbers r0 and r1.
.macro BOOT_MARKER letter
    movw    r0, #:lower16:MARKER_UTXH
    movt    r0, #:upper16:MARKER_UTXH
    add     r1, r10, #'0'
    strb    r1, [r0]
    mov     r1, #\letter
    strb    r1, [r0]
.endm

.pushsection .text.boot, "ax", %progbits

.global _start
.type _start, %function
_start:
    @ zImage header. The board's signed U-Boot has no available source; the one known
    @ good format is the Linux zImage it boots with `bootm`, so this copies its layout:
    @ `file`/dump of output/images/zImage, and
    @ linux-5.15.198/arch/arm/boot/compressed/head.S:219-222, vmlinux.lds.S:121-123.
    @ Tested on the board: `bootm` accepts it from a boot.img and prints "Boot with zImage".
    .rept 8
    mov     r0, r0
    .endr
    b       __exynos_boot                   @ 0x20
    .word   0x016f2818                      @ 0x24 zImage magic
    .word   0                               @ 0x28 start: position independent
    .word   __image_size                    @ 0x2c end: bytes in the image
    .word   0x04030201                      @ 0x30 little-endian marker

__exynos_boot:
    @ 1. Keep U-Boot's r0-r2 in callee-saved registers: there is no stack yet.
    mov     r4, r0
    mov     r5, r1
    mov     r6, r2

    @ 2. Mask IRQ/FIQ and enter SVC mode.
    cpsid   if, #MODE_SVC

    @ 3. Only CPU0 continues. U-Boot only ever runs CPU0, so this is a safety net: real
    @    secondaries are started by `smp::start_cpu` at __exynos_secondary_entry.
    mrc     p15, 0, r0, c0, c0, 5           @ MPIDR
    ands    r0, r0, #0xff                   @ Cortex-A9: CPU ID lives in Aff0
    bne     __exynos_park_secondary

    @ 4. MMU and caches off. `bootm` has already done this for Linux, so no dirty
    @    lines are cleaned here. After a `go` it needs `dcache off` in U-Boot first.
    @    The PL310 L2 is off at this point on the board (`mmu_test`: L2 CTRL = 0).
    QUIESCE_CPU

    @ Run from the link address. On the board, the boot.img header stays in the
    @ partition and `bootm` starts the image one 4096-byte page later, at 0x40009000.
    @ Only branches and PC-relative code have run so far.
    adr     r7, _start
    movw    r1, #:lower16:_start
    movt    r1, #:upper16:_start
    cmp     r7, r1
    beq     __exynos_relocated
    bhi     __exynos_relocate
    b       __exynos_halt                   @ below the link address: unsupported

__exynos_relocated:
    @ 5. Vector table. Nothing below is expected to fault before the stacks exist.
    movw    r0, #:lower16:__exynos_vectors
    movt    r0, #:upper16:__exynos_vectors
    mcr     p15, 0, r0, c12, c0, 0          @ VBAR
    isb

    @ 6. One stack per mode, finishing in SVC where the application runs.
    cps     #MODE_FIQ
    movw    sp, #:lower16:__fiq_stack_top
    movt    sp, #:upper16:__fiq_stack_top
    cps     #MODE_IRQ
    movw    sp, #:lower16:__irq_stack_top
    movt    sp, #:upper16:__irq_stack_top
    cps     #MODE_ABT
    movw    sp, #:lower16:__abt_stack_top
    movt    sp, #:upper16:__abt_stack_top
    cps     #MODE_UND
    movw    sp, #:lower16:__und_stack_top
    movt    sp, #:upper16:__und_stack_top
    cps     #MODE_SVC
    movw    sp, #:lower16:__svc_stack_top
    movt    sp, #:upper16:__svc_stack_top

    @ 7. Zero .bss.
    movw    r0, #:lower16:__bss_start
    movt    r0, #:upper16:__bss_start
    movw    r1, #:lower16:__bss_end
    movt    r1, #:upper16:__bss_end
    mov     r2, #0
1:  cmp     r0, r1
    strlo   r2, [r0], #4
    blo     1b

    @ 8. VFP/NEON, see the macro.
    ENABLE_VFP __exynos_halt

    @ 9. Hand over to the function generated by `entry!`; it never returns.
    mov     r0, r4
    mov     r1, r5
    mov     r2, r6
    mov     r3, r7                          @ address the image was started from
    bl      __exynos_main
    b       __exynos_halt
.size _start, . - _start

@ Where the firmware sends CPU1-3 after `smp::start_cpu` (Non-secure, SVC or similar,
@ MMU off). This core must not touch normal memory until its MMU is on: CPU0 runs with
@ caches on, and it may hold a stale line for anything written directly to DRAM. So the
@ stack registers are only loaded here, and first used after __exynos_mmu_on.
.global __exynos_secondary_entry
.type __exynos_secondary_entry, %function
__exynos_secondary_entry:
    cpsid   if, #MODE_SVC
    QUIESCE_CPU

    mrc     p15, 0, r0, c0, c0, 5           @ MPIDR
    and     r10, r0, #0xff
    cmp     r10, #0
    beq     __exynos_halt
    cmp     r10, #4
    bhs     __exynos_halt

    movw    r0, #:lower16:__exynos_vectors
    movt    r0, #:upper16:__exynos_vectors
    mcr     p15, 0, r0, c12, c0, 0          @ VBAR
    isb

    movw    r0, #:lower16:__stacks_per_cpu
    movt    r0, #:upper16:__stacks_per_cpu
    mul     r2, r0, r10                     @ this CPU's offset from CPU0's block
    cps     #MODE_FIQ
    movw    sp, #:lower16:__fiq_stack_top
    movt    sp, #:upper16:__fiq_stack_top
    add     sp, sp, r2
    cps     #MODE_IRQ
    movw    sp, #:lower16:__irq_stack_top
    movt    sp, #:upper16:__irq_stack_top
    add     sp, sp, r2
    cps     #MODE_ABT
    movw    sp, #:lower16:__abt_stack_top
    movt    sp, #:upper16:__abt_stack_top
    add     sp, sp, r2
    cps     #MODE_UND
    movw    sp, #:lower16:__und_stack_top
    movt    sp, #:upper16:__und_stack_top
    add     sp, sp, r2
    cps     #MODE_SVC
    movw    sp, #:lower16:__svc_stack_top
    movt    sp, #:upper16:__svc_stack_top
    add     sp, sp, r2

    ENABLE_VFP __exynos_secondary_no_vfp

    bl      __exynos_mmu_on
    cmp     r0, #0
    bne     __exynos_secondary_no_smp

    mov     r0, r10
    bl      __exynos_secondary_rust
    b       __exynos_halt
.size __exynos_secondary_entry, . - __exynos_secondary_entry

__exynos_secondary_no_vfp:
    BOOT_MARKER 'V'
    b       __exynos_halt

__exynos_secondary_no_smp:
    BOOT_MARKER 'S'
    b       __exynos_halt

@ Turns on this core's L1 caches and MMU over the shared table, from any core. Returns
@ r0 = 0, or r0 = 1 without touching the MMU if ACTLR.SMP cannot be set. Uses no stack,
@ for the reason above. Clobbers r0-r5, r12.
.global __exynos_mmu_on
.type __exynos_mmu_on, %function
__exynos_mmu_on:
    @ Invalidate, without cleaning, the whole L1 data cache by set and way: `cp15` c7,
    @ c6, 2 and the CCSIDR fields as in arch/arm/mm/cache-v7.S, v7_invalidate_l1. The
    @ cache is off and memory has been written behind it, so its contents are worthless.
    mov     r0, #0
    mcr     p15, 2, r0, c0, c0, 0           @ CSSELR: L1 data
    isb
    mrc     p15, 1, r0, c0, c0, 0           @ CCSIDR
    and     r1, r0, #7
    add     r1, r1, #4                      @ r1 = set shift
    ubfx    r2, r0, #3, #10                 @ r2 = way, counting down
    ubfx    r3, r0, #13, #15                @ r3 = highest set
    clz     r4, r2                          @ r4 = way shift
1:  mov     r5, r3
2:  lsl     r12, r2, r4
    orr     r12, r12, r5, lsl r1
    mcr     p15, 0, r12, c7, c6, 2          @ DCISW
    subs    r5, r5, #1
    bpl     2b
    subs    r2, r2, #1
    bpl     1b
    dsb

    @ Join coherency before the caches come on. Written only if it changes, as in
    @ proc-v7.S:307-311: NSACR decides whether Non-secure may write this at all.
    mrc     p15, 0, r0, c1, c0, 1           @ ACTLR
    orr     r1, r0, #(ACTLR_SMP | ACTLR_FW)
    cmp     r1, r0
    mcrne   p15, 0, r1, c1, c0, 1
    isb
    mrc     p15, 0, r0, c1, c0, 1
    tst     r0, #ACTLR_SMP
    moveq   r0, #1
    bxeq    lr

    @ TTBCR 0 = TTBR0 for all addresses (proc-v7-2level.S:145), domain 0 client,
    @ TTBR0 attributes 0 = non-cacheable walks, the table is never written.
    @ ICIALLU, BPIALL and TLBIALL drop state left by whoever ran here before.
    movw    r1, #:lower16:__exynos_mmu_table_addr
    movt    r1, #:upper16:__exynos_mmu_table_addr
    ldr     r1, [r1]
    mov     r0, #0
    mov     r2, #DACR_DOMAIN0_CLIENT
    mcr     p15, 0, r0, c2, c0, 2           @ TTBCR
    mcr     p15, 0, r2, c3, c0, 0           @ DACR
    mcr     p15, 0, r1, c2, c0, 0           @ TTBR0
    mcr     p15, 0, r0, c7, c5, 0           @ ICIALLU
    mcr     p15, 0, r0, c7, c5, 6           @ BPIALL
    mcr     p15, 0, r0, c8, c7, 0           @ TLBIALL
    dsb
    isb
    mrc     p15, 0, r1, c1, c0, 0           @ SCTLR
    bic     r1, r1, #(SCTLR_TRE | SCTLR_AFE)
    orr     r1, r1, #(SCTLR_M | SCTLR_C)
    orr     r1, r1, #(SCTLR_Z | SCTLR_I)
    mcr     p15, 0, r1, c1, c0, 0
    isb
    bx      lr
.size __exynos_mmu_on, . - __exynos_mmu_on

.type __exynos_park_secondary, %function
__exynos_park_secondary:
    wfe
    b       __exynos_park_secondary
.size __exynos_park_secondary, . - __exynos_park_secondary

.type __exynos_halt, %function
__exynos_halt:
    wfi
    b       __exynos_halt
.size __exynos_halt, . - __exynos_halt

.popsection

@ Copies the image down to its link address, then continues there. Placed last in
@ the image by link.x: with a downward copy, the destination stops `start - link`
@ bytes below the end of the source, so a large enough offset leaves the tail intact.
.pushsection .relocate_code, "ax", %progbits

.type __exynos_relocate, %function
__exynos_relocate:
    sub     r0, r7, r1                      @ r7 = source, r1 = destination
    cmp     r0, #RELOCATE_MIN_OFFSET
    blo     __exynos_halt                   @ the copy would overwrite its own loop
    tst     r7, #3
    bne     __exynos_halt
    movw    r2, #:lower16:__image_size
    movt    r2, #:upper16:__image_size
    mov     r0, r7
.global __exynos_relocate_tail
__exynos_relocate_tail:
    ldr     r3, [r0], #4
    str     r3, [r1], #4
    subs    r2, r2, #4
    bhi     __exynos_relocate_tail
    dsb
    mov     r0, #0
    mcr     p15, 0, r0, c7, c5, 0           @ ICIALLU
    mcr     p15, 0, r0, c7, c5, 6           @ BPIALL
    isb
    movw    r0, #:lower16:__exynos_relocated
    movt    r0, #:upper16:__exynos_relocated
    bx      r0
.size __exynos_relocate, . - __exynos_relocate

.popsection

.pushsection .text.vectors, "ax", %progbits

.p2align 5
.global __exynos_vectors
.type __exynos_vectors, %object
__exynos_vectors:
    b       _start                          @ 0x00 reset
    b       __exynos_exc_undef              @ 0x04 undefined instruction
    b       __exynos_exc_svc                @ 0x08 supervisor call
    b       __exynos_exc_prefetch_abort     @ 0x0c prefetch abort
    b       __exynos_exc_data_abort         @ 0x10 data abort
    b       .                               @ 0x14 reserved
    b       __exynos_exc_irq                @ 0x18 IRQ
    b       __exynos_exc_fiq                @ 0x1c FIQ
.size __exynos_vectors, . - __exynos_vectors

@ Builds the `ExceptionFrame` (see exceptions.rs), calls `\handler(&mut frame)`
@ and resumes at frame.return_addr with frame.spsr.
@ `\adjust` turns the banked LR into that resume address.
.macro EXCEPTION_STUB label, adjust, mode, handler
.type \label, %function
\label:
    sub     lr, lr, #\adjust
    srsdb   sp!, #\mode                     @ push return_addr, spsr
    push    {r0-r12, lr}
    vmrs    r0, fpscr
    push    {r0}
    vpush   {d16-d31}                       @ AAPCS caller-saved, used by generated code
    vpush   {d0-d7}
    mov     r0, sp                          @ argument: frame pointer
    mov     r4, sp                          @ callee-saved copy of the frame pointer
    bic     sp, sp, #7                      @ AAPCS: 8-byte aligned at the call
    bl      \handler
    mov     sp, r4
    vpop    {d0-d7}
    vpop    {d16-d31}
    pop     {r0}
    vmsr    fpscr, r0
    pop     {r0-r12, lr}
    clrex                                   @ the interrupted LDREX must not succeed
    rfeia   sp!
.size \label, . - \label
.endm

@ Banked LR offsets (ARMv7-A ARM, exception return table):
@   UND, SVC: LR already holds the next instruction.
@   prefetch abort: LR-4 is the faulting instruction.
@   data abort: LR-8 is the faulting instruction.
@   IRQ, FIQ: LR-4 is the instruction to resume.
EXCEPTION_STUB __exynos_exc_undef,          0, MODE_UND, handle_undef
EXCEPTION_STUB __exynos_exc_svc,            0, MODE_SVC, handle_svc
EXCEPTION_STUB __exynos_exc_prefetch_abort, 4, MODE_ABT, handle_prefetch_abort
EXCEPTION_STUB __exynos_exc_data_abort,     8, MODE_ABT, handle_data_abort
EXCEPTION_STUB __exynos_exc_irq,            4, MODE_IRQ, handle_irq
EXCEPTION_STUB __exynos_exc_fiq,            4, MODE_FIQ, handle_fiq

.popsection
