/*
 * ORIGIN is the load address, the only place the application sets it. It must match
 * the board's bootcmd: mmc read 0 40008000 10040 4FC0; bootm 40008000 41000000
 */
MEMORY
{
    RAM : ORIGIN = 0x40008000, LENGTH = 2M
}
