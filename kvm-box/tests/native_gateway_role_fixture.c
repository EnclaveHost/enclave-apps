/* Test-only exec adapter for the production native role policy and gateway.
 * Real M3/nsinit integration will call the same policy before fixed execs.
 */
#define _GNU_SOURCE
#include "../guest/native/role-policy.h"
#include <fcntl.h>
int main(int argc, char **argv) {
    (void)argv;
    if (argc != 1 || getuid() || geteuid()) return 64;
    int fd = open("/dev/null", O_RDWR | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) return 65;
    for (int i = 0; i < 3; ++i) if (dup2(fd, i) < 0 || fcntl(i, F_SETFD, 0)) return 65;
    if (syscall(SYS_close_range, 3U, ~0U, 0) || native_enter_role(1002)) return 66;
    char *const args[] = { "/gateway", NULL };
    char *const env[] = { "PATH=/usr/bin:/bin", NULL };
    execve(args[0], args, env);
    return 127;
}
