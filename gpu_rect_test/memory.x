/*
 * ORIGIN is the load address: see hello_world/memory.x. The framebuffer, 1280 x 800 x 4 bytes,
 * is in .bss: 16 MiB leaves room for it, the streams and the GPU's page tables.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 16M
}
