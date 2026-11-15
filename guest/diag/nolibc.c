/* Diagnostic for #54: prints one line on the console with raw syscalls and no C library, then waits forever.
 * If this line never shows up, the x86_64 guest's console write path is broken. Temporary; not merged. */

static long syscall3(long number, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(number), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return ret;
}

static const char message[] = "hermit-agent: ready (control protocol 0.1) probe=nolibc\n";

void _start(void) {
    syscall3(1, 1, (long)message, sizeof message - 1); /* write(1, ...) */
    for (;;) {
        syscall3(34, 0, 0, 0); /* pause() */
    }
}
