/*
 * ORIGIN is the load address, as for every application: bootm starts this from the boot
 * partition like any other. The images it receives go to STAGE, 0x50000000, well above.
 *
 * 4 MiB: the FAT32 device buffer (~100 KiB, cache.rs) and the stacks of the four cores
 * fit easily, still far below STAGE.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 4M
}

/* embedded-sdmmc's VolumeManager, with its blocks, lives on the stack (as fat_test). */
__svc_stack_size = 64K;
