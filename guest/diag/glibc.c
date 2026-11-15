/* Diagnostic for #54: the same static glibc and flags as the agent, printing one line, then waiting forever.
 * If the no-libc probe prints and this one does not, glibc's static startup is the problem. Temporary; not merged. */
#include <stdio.h>
#include <unistd.h>

int main(void) {
    fputs("hermit-agent: ready (control protocol 0.1) probe=glibc\n", stdout);
    fflush(stdout);
    for (;;) {
        pause();
    }
}
