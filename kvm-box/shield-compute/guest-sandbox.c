// Applied after dropping uid, before either desktop or trusted compute starts.
// It is inherited across exec and cannot be removed by unprivileged children.
#define _GNU_SOURCE
#include <errno.h>
#include <seccomp.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 2 || getuid() == 0 || geteuid() == 0) return 64;
    scmp_filter_ctx filter = seccomp_init(SCMP_ACT_ALLOW);
    if (!filter || prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) return 77;
    int rc = 0;
    rc |= seccomp_rule_add(filter, SCMP_ACT_ERRNO(EPERM), SCMP_SYS(socket), 1,
                           SCMP_A0(SCMP_CMP_EQ, AF_VSOCK));
    rc |= seccomp_rule_add(filter, SCMP_ACT_ERRNO(EPERM), SCMP_SYS(socket), 1,
                           SCMP_A0(SCMP_CMP_EQ, AF_PACKET));
    // io_uring can create sockets without executing socket(2).
    rc |= seccomp_rule_add(filter, SCMP_ACT_ERRNO(EPERM), SCMP_SYS(io_uring_setup), 0);
    rc |= seccomp_rule_add(filter, SCMP_ACT_ERRNO(EPERM), SCMP_SYS(io_uring_enter), 0);
    rc |= seccomp_rule_add(filter, SCMP_ACT_ERRNO(EPERM), SCMP_SYS(io_uring_register), 0);
    if (rc || seccomp_load(filter)) { seccomp_release(filter); return 77; }
    seccomp_release(filter);
    execvp(argv[1], argv + 1);
    perror("sandbox exec");
    return 127;
}
