/*
 * ORIGIN is the load address, as for every application: bootm starts this from the boot
 * partition like any other. The images it receives go to STAGE, 0x50000000, well above.
 *
 * 4 MiB: the WiFi firmware (~0.5 MiB), the FAT32 device buffer (~100 KiB, cache.rs), the
 * network's buffers and the stacks of the four cores fit easily, still far below STAGE.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 4M
}

/* embedded-sdmmc's VolumeManager, with its blocks, lives on the stack (as fat_test), and so do
 * the frames of joining WiFi: 69 KiB for join alone (wifi_async_test/memory.x). */
__svc_stack_size = 192K;
