/*
 * ORIGIN is the load address, the only place the application sets it. It must match
 * the board's bootcmd: mmc read 0 40008000 10040 4FC0; bootm 40008000 41000000
 *
 * DRAM is 0x40000000 + 0x7EA00000 (exynos4412-mehmet.dts, memory@40000000). 15 MiB holds
 * the 12 MiB heap and still ends below the 0x41000000 that bootcmd passes as the ramdisk
 * address, and far from the top of DRAM, where U-Boot conventionally relocates itself. It
 * also stays inside the first 16 MiB of DRAM, which mmu::DRAM_UNCACHED covers.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 15M
}

/* ext4-view's calls nest a few levels per path component; 256K leaves room. */
__svc_stack_size = 256K;
