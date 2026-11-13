/*
 * ORIGIN is the load address, the only place the application sets it. It must match
 * the board's bootcmd: mmc read 0 40008000 10040 4FC0; bootm 40008000 41000000
 *
 * DRAM is 0x40000000 + 0x7EA00000 (exynos4412-mehmet.dts, memory@40000000). 12 MiB
 * holds the image with its 1 MiB of test pictures and the 4 MiB framebuffer in .bss,
 * inside the first 16 MiB of DRAM that the uncached view covers. The codec's buffers
 * are elsewhere: WORK in main.rs.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 12M
}
