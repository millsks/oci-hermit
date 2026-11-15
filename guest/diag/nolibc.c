/* Diagnostic for #54, with raw syscalls and no C library. Temporary; not merged.
 *
 * Default build: prints one line on its stdout (the /dev/console the kernel opened for init), then waits forever.
 *
 * With -DEXIT_AFTER_WRITE: reports each step through its exit status, which the kernel prints in its
 * "Attempted to kill init! exitcode=0x0000XX00" panic, XX being:
 *   bit 0  write to stdout did not write the whole line
 *   bit 1  mounting devtmpfs on /dev failed
 *   bit 2  opening /dev/ttyS0 failed
 *   bit 3  write to /dev/ttyS0 did not write the whole line
 *   bit 4  stdout write returned an error (as opposed to a short write)
 */

static long syscall5(long number, long a, long b, long c, long d, long e) {
    long ret;
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8)
                     : "rcx", "r11", "memory");
    return ret;
}

static const char message[] = "hermit-agent: ready (control protocol 0.1) probe=nolibc\n";
static const char direct[] = "hermit-agent: ready (control protocol 0.1) probe=nolibc-direct-ttyS0\n";

__attribute__((force_align_arg_pointer)) void _start(void) {
    long len = sizeof message - 1;
    long wrote = syscall5(1, 1, (long)message, len, 0, 0); /* write(1, ...) */
#ifdef EXIT_AFTER_WRITE
    long status = 0;
    if (wrote != len) status |= 1;
    if (wrote < 0) status |= 16;
    if (syscall5(165, (long)"devtmpfs", (long)"/dev", (long)"devtmpfs", 0, 0) != 0) status |= 2; /* mount */
    long fd = syscall5(2, (long)"/dev/ttyS0", 01 | 0400, 0, 0, 0); /* open O_WRONLY|O_NOCTTY */
    if (fd < 0) {
        status |= 4 | 8;
    } else if (syscall5(1, fd, (long)direct, sizeof direct - 1, 0, 0) != (long)(sizeof direct - 1)) {
        status |= 8;
    }
    /* Give the UART time to drain before the panic, so a delivered line has a chance to appear. */
    struct { long sec, nsec; } nap = {2, 0};
    syscall5(35, (long)&nap, 0, 0, 0, 0); /* nanosleep */
    syscall5(60, status, 0, 0, 0, 0);    /* exit */
#endif
    (void)wrote;
    for (;;) {
        syscall5(34, 0, 0, 0, 0, 0); /* pause() */
    }
}
