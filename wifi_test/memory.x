/*
 * ORIGIN is the load address, the only place the application sets it. It must match
 * the board's bootcmd: mmc read 0 40008000 10040 4FC0; bootm 40008000 41000000
 *
 * DRAM is 0x40000000 + 0x7EA00000 (exynos4412-mehmet.dts, memory@40000000). 4 MiB is
 * far more than this program needs. It keeps the image, .bss and stacks at the bottom
 * of DRAM, below the 0x41000000 that bootcmd passes as the ramdisk address and far
 * from the top of DRAM, where U-Boot conventionally relocates itself. The exact U-Boot
 * address is unknown: its source is not available.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 4M
}

/* Frames, the table of networks and the handshake's buffers all live on the stack of main. */
__svc_stack_size = 32K;
