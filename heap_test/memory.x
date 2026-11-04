/*
 * ORIGIN is the load address: see hello_world/memory.x. The image, .bss and the stacks of the
 * four cores take well under 8 MiB; the heap follows them, 1 GiB, ending at 0x80808000, far
 * below the end of DRAM at 0xBEA00000 (README, "Information taken from the DTS").
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 1032M
}

__heap_size = 1024M;
