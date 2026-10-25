/* Test-only nsinit: creates a descendant that escapes its process group.
 * Never package this as a desktop entry point. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>
static void stop(int sig) { (void)sig; _exit(0); }
int main(void) {
    if (getpid() != 1 || getuid() != 0) return 64;
    for (int i = 0; i < 3; ++i) {
        struct stat st;
        if (fstat(i, &st) || !S_ISCHR(st.st_mode) || st.st_rdev != makedev(1, 3) ||
            fcntl(i, F_GETFD) != 0) return 72;
    }
    char mode[32] = {0};
    int fd = open("/fixture-mode", O_RDONLY);
    if (fd < 0 || read(fd, mode, sizeof mode - 1) < 1) return 65;
    close(fd);
    if (!strcmp(mode, "mount-check")) {
        if (mount("tmpfs", "/mount-probe", "tmpfs", MS_NOSUID | MS_NODEV, "size=32k")) return 73;
        fd = open("/mount-probe/child-only", O_WRONLY | O_CREAT, 0600);
        if (fd < 0) return 74;
        close(fd);
    }
    signal(SIGTERM, stop);
    pid_t child = fork();
    if (child < 0) return 66;
    if (!child) {
        close(3); close(4);
        if (setgid(1000) || setuid(1000) || setsid() < 0) return 67;
        if (prctl(PR_SET_NAME, "kvm-ns-escape", 0, 0, 0)) return 68;
        signal(SIGTERM, SIG_IGN);
        for (;;) pause();
    }
    if (!strcmp(mode, "never-ready")) {
        /* Keep the readiness pipe open without writing. The production
         * supervisor deadline must kill this namespace and its descendant. */
    } else if (!strcmp(mode, "split-invalid")) {
        if (write(3, "R", 1) != 1) return 69;
        usleep(50000);
        if (write(3, "X", 1) != 1) return 69;
        close(3);
    } else if (!strcmp(mode, "bad-ready")) {
        if (write(3, "RR", 2) != 2) return 69;
        close(3);
    } else {
        if (write(3, "R", 1) != 1) return 69;
        close(3);
        if (!strcmp(mode, "die")) { usleep(200000); return 70; }
    }
    struct pollfd parent = { .fd = 4, .events = POLLIN };
    for (;;) {
        if (poll(&parent, 1, 50) > 0) return 71;
        while (waitpid(-1, NULL, WNOHANG) > 0) {}
    }
}
