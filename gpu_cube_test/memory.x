/*
 * ORIGIN is the load address: see hello_world/memory.x. The GPU's buffers and page tables are
 * in .bss with the 4 MiB framebuffer (gpu::memory: about 14 MiB), under the 24 MiB here.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 24M
}
