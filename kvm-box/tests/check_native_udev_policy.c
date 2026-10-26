#define _GNU_SOURCE
#include "../guest/native/udev-policy.h"
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>

static int failures;
static void expect_denied(const char *name, long result) {
    if (result != -1 || errno != EPERM) {
        fprintf(stderr, "FAIL %s: result=%ld errno=%d\n", name, result, errno); ++failures;
    }
}
static void check_inherited(void) {
    struct __user_cap_header_struct h = { .version = _LINUX_CAPABILITY_VERSION_3 };
    struct __user_cap_data_struct d[2];
    if (syscall(SYS_capget, &h, d) || d[0].effective != (uint32_t)UDEV_CAPS ||
        d[0].permitted != (uint32_t)UDEV_CAPS || d[0].inheritable ||
        d[1].effective || d[1].permitted || d[1].inheritable ||
        prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 1 || prctl(PR_GET_SECCOMP) != 2) {
        fprintf(stderr, "FAIL inherited capabilities / NNP / seccomp\n"); ++failures; return;
    }
    for (int cap = 0; cap <= CAP_LAST_CAP; ++cap)
        if (prctl(PR_CAPBSET_READ, cap) != !!(UDEV_CAPS & (1ULL << cap))) ++failures;
    expect_denied("IPv4", socket(AF_INET, SOCK_STREAM, 0));
    expect_denied("IPv6", socket(AF_INET6, SOCK_STREAM, 0));
    expect_denied("vsock", socket(AF_VSOCK, SOCK_STREAM, 0));
    expect_denied("packet", socket(AF_PACKET, SOCK_RAW, 0));
    expect_denied("netfilter", socket(AF_NETLINK, SOCK_RAW, NETLINK_NETFILTER));
    expect_denied("route", socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
    expect_denied("generic netlink", socket(AF_NETLINK, SOCK_RAW, NETLINK_GENERIC));
    expect_denied("ioctl", syscall(SYS_ioctl, -1, 0, 0));
    expect_denied("mknod", syscall(SYS_mknod, NULL, 0, 0));
    expect_denied("mknodat", syscall(SYS_mknodat, -1, NULL, 0, 0));
    expect_denied("io_uring", syscall(SYS_io_uring_setup, 1, NULL));
    expect_denied("unshare", syscall(SYS_unshare, CLONE_NEWUSER));
    expect_denied("setns", syscall(SYS_setns, -1, 0));
    expect_denied("mount", syscall(SYS_mount, NULL, NULL, NULL, 0, NULL));
    expect_denied("bpf", syscall(SYS_bpf, 0, NULL, 0));
    expect_denied("ptrace", syscall(SYS_ptrace, 0, 0, 0, 0));
    expect_denied("pidfd_getfd", syscall(SYS_pidfd_getfd, -1, 0, 0));
    expect_denied("clone namespace", syscall(SYS_clone, CLONE_NEWUSER | SIGCHLD, NULL, NULL, NULL, 0));
    errno = 0;
    if (syscall(SYS_clone3, NULL, 0) != -1 || errno != ENOSYS) ++failures;
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair)) ++failures;
    else { close(pair[0]); close(pair[1]); }
    int fd = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) ++failures; else close(fd);
    pid_t child = fork();
    if (child == 0) { syscall(0x40000000 | SYS_getpid); _exit(7); }
    int status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child || !WIFSIGNALED(status) || WTERMSIG(status) != SIGSYS) ++failures;
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "inherited")) {
        check_inherited();
        puts(failures ? "udev policy checks FAILED" : "udev policy exec inheritance and syscall checks passed");
        return failures ? 1 : 0;
    }
    /* No kernel events or multicast may escape into the user's network ns. */
    char current[128], line[128];
    const char *parent = getenv("KVMBOX_TEST_PARENT_NETNS");
    ssize_t n = readlink("/proc/self/ns/net", current, sizeof current - 1);
    FILE *map = fopen("/proc/self/uid_map", "r");
    unsigned inside, outside, count;
    if (argc != 1 || getuid() || getpid() != 1 || !parent || n < 0 || !map ||
        !fgets(line, sizeof line, map) || sscanf(line, "%u %u %u", &inside, &outside, &count) != 3 ||
        inside || !outside || count != 1) return 64;
    fclose(map); current[n] = 0;
    if (!strcmp(parent, current)) return 64;
    int monitor = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_KOBJECT_UEVENT);
    struct sockaddr_nl addr = { .nl_family = AF_NETLINK, .nl_groups = 2 };
    struct timeval timeout = { .tv_sec = 3 };
    if (monitor < 0 || bind(monitor, (struct sockaddr *)&addr, sizeof addr) ||
        setsockopt(monitor, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof timeout)) return 65;
    pid_t child = fork();
    if (child == 0) {
        close(monitor);
        if (udev_drop_capabilities() || udev_install_seccomp()) { perror("install policy"); _exit(66); }
        int event = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_KOBJECT_UEVENT);
        const char payload[] = "kvm-box-private-udev-test";
        if (event < 0 || sendto(event, payload, sizeof payload, 0, (struct sockaddr *)&addr, sizeof addr) != sizeof payload) {
            perror("confined udev multicast"); _exit(67);
        }
        close(event);
        execl("/proc/self/exe", "udev-policy-test", "inherited", NULL);
        _exit(68);
    }
    char received[128];
    n = recv(monitor, received, sizeof received, 0); close(monitor);
    int status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child || status ||
        n != sizeof "kvm-box-private-udev-test" || memcmp(received, "kvm-box-private-udev-test", (size_t)n)) return 1;
    puts("confined udev multicast received in private netns");
    return 0;
}
