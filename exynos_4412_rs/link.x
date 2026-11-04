/*
 * Linker script for exynos_4412_rs. The application selects it with -Tlink.x
 * and supplies memory.x, which must define a RAM region whose ORIGIN is the
 * load address, for example:
 *
 *     MEMORY { RAM : ORIGIN = 0x40008000, LENGTH = 4M }
 *
 * memory.x may also override any stack size below, e.g. __svc_stack_size = 64K;
 * and give the heap of the `heap` feature its size, e.g. __heap_size = 64M; it goes after
 * the stacks, so RAM must be that much longer.
 */
INCLUDE memory.x

ENTRY(_start);

/* Pull the runtime out of the rlib even though nothing in Rust references it. */
EXTERN(__exynos_vectors);
EXTERN(__exynos_relocate_tail);
EXTERN(__default_handle_undef);
EXTERN(__default_handle_svc);
EXTERN(__default_handle_prefetch_abort);
EXTERN(__default_handle_data_abort);
EXTERN(__default_handle_irq);
EXTERN(__default_handle_fiq);

/* An application overrides a handler by defining the left-hand symbol. */
PROVIDE(handle_undef = __default_handle_undef);
PROVIDE(handle_svc = __default_handle_svc);
PROVIDE(handle_prefetch_abort = __default_handle_prefetch_abort);
PROVIDE(handle_data_abort = __default_handle_data_abort);
PROVIDE(handle_irq = __default_handle_irq);
PROVIDE(handle_fiq = __default_handle_fiq);

PROVIDE(__svc_stack_size = 16K);
PROVIDE(__irq_stack_size = 16K);
PROVIDE(__abt_stack_size = 16K);
PROVIDE(__und_stack_size = 16K);
PROVIDE(__fiq_stack_size = 16K);
PROVIDE(__heap_size = 0);

SECTIONS
{
    .text ORIGIN(RAM) :
    {
        __text_start = .;
        KEEP(*(.text.boot));
        KEEP(*(.text.vectors));
        *(.text .text.*);
        . = ALIGN(4);
        __text_end = .;
    } > RAM

    .rodata : ALIGN(4)
    {
        *(.rodata .rodata.*);
        . = ALIGN(4);
    } > RAM

    /* Loaded in place by U-Boot as part of the image: no LMA/VMA split, no copy. */
    .data : ALIGN(4)
    {
        *(.data .data.*);
        . = ALIGN(4);
    } > RAM

    /* Must stay last in the image, see __exynos_relocate in startup.s. */
    .relocate : ALIGN(4)
    {
        KEEP(*(.relocate_code));
        . = ALIGN(4);
        __image_end = .;
    } > RAM
    __image_size = __image_end - ORIGIN(RAM);

    .bss (NOLOAD) : ALIGN(4)
    {
        __bss_start = .;
        *(.bss .bss.*);
        *(COMMON);
        . = ALIGN(4);
        __bss_end = .;
    } > RAM

    /*
     * Full-descending stacks, lowest first; each top stays 8-byte aligned (AAPCS).
     * The symbols describe CPU0's block. CPU n uses the same layout __stacks_per_cpu * n
     * bytes higher: see __exynos_secondary_entry in startup.s.
     */
    .stacks (NOLOAD) : ALIGN(8)
    {
        __stacks_start = .;
        . += ALIGN(__svc_stack_size, 8);
        __svc_stack_top = .;
        . += ALIGN(__irq_stack_size, 8);
        __irq_stack_top = .;
        . += ALIGN(__abt_stack_size, 8);
        __abt_stack_top = .;
        . += ALIGN(__und_stack_size, 8);
        __und_stack_top = .;
        . += ALIGN(__fiq_stack_size, 8);
        __fiq_stack_top = .;
        __stacks_per_cpu = . - __stacks_start;
        . += 3 * __stacks_per_cpu;
        __stacks_end = .;
    } > RAM

    /* Not cleared at boot: memory the allocator hands out is uninitialised anyway. */
    .heap (NOLOAD) : ALIGN(4096)
    {
        __heap_start = .;
        . += __heap_size;
        __heap_end = .;
    } > RAM

    /* Unwind tables are useless with panic=abort and would land in the binary. */
    /DISCARD/ :
    {
        *(.ARM.exidx .ARM.exidx.*);
        *(.ARM.extab .ARM.extab.*);
    }
}

ASSERT(_start == ORIGIN(RAM), "link.x: _start must be exactly at the load address");
ASSERT(__image_end - __exynos_relocate_tail <= 0x40, "link.x: the relocation loop must fit in the bytes a 0x40 offset leaves intact");
ASSERT(__exynos_vectors % 32 == 0, "link.x: VBAR requires a 32-byte aligned vector table");
ASSERT(__bss_start % 4 == 0 && __bss_end % 4 == 0, "link.x: .bss must be word aligned");
ASSERT(__heap_end <= ORIGIN(RAM) + LENGTH(RAM), "link.x: the heap does not fit in RAM; enlarge RAM in memory.x or shrink __heap_size");
ASSERT(__stacks_end <= ORIGIN(RAM) + LENGTH(RAM), "link.x: the stacks do not fit in RAM; enlarge RAM in memory.x or shrink __*_stack_size");
