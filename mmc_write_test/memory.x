/*
 * ORIGIN is the load address, the only place the application sets it. It must match
 * the board's bootcmd: mmc read 0 40008000 10040 4FC0; bootm 40008000 41000000
 *
 * DRAM is 0x40000000 + 0x7EA00000 (exynos4412-mehmet.dts, memory@40000000). 8 MiB holds
 * three 2 MiB buffers and stays below the 0x41000000 that bootcmd passes as the ramdisk
 * address, and inside the first 16 MiB of DRAM, which mmu::DRAM_UNCACHED covers.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 8M
}
