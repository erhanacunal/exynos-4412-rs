/*
 * ORIGIN is the load address, as for every application: bootm starts this from the boot
 * partition like any other. The images it receives go to STAGE, 0x50000000, well above.
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 1M
}
