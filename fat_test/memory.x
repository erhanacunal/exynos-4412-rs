/*
 * ORIGIN is the load address, the only place the application sets it. It must match
 * the board's bootcmd: mmc read 0 40008000 10040 4FC0; bootm 40008000 41000000
 *
 * DRAM is 0x40000000 + 0x7EA00000 (exynos4412-mehmet.dts, memory@40000000). 4 MiB holds
 * the device's 264 KiB buffer and the 32 KiB chunk easily and stays below the 0x41000000 that bootcmd passes as the
 * ramdisk address.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 4M
}

/* embedded-sdmmc's VolumeManager, with its block, lives on the stack. */
__svc_stack_size = 64K;
