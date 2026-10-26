/* Disposable VM fixture only. No owner credential or release bypass.
 * Join the controller network namespace and drop identity before executing
 * the fixed HTTP test client. Never included in the production image. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <grp.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 3 || getuid() || geteuid()) return 64;
    char *end; long pid = strtol(argv[1], &end, 10);
    if (*end || pid < 2 || pid > 2147483647) return 64;
    long uid = strtol(argv[2], &end, 10);
    if (*end || (uid != 5001 && (uid < 1000 || uid > 1003))) return 64;
    char cmdline[4096] = {0}; int f = open("/proc/cmdline", O_RDONLY | O_CLOEXEC);
    if (f < 0) return 65;
    ssize_t size = read(f, cmdline, sizeof cmdline - 1); close(f);
    if (size <= 0 || !strstr(cmdline, "native.monitor.probe=1")) return 65;
    char path[96]; snprintf(path, sizeof path, "/proc/%ld/ns/net", pid);
    int net = open(path, O_RDONLY | O_CLOEXEC);
    if (net < 0 || setns(net, CLONE_NEWNET)) return 66;
    close(net);
    if (setgroups(0, NULL) || setresgid(uid, uid, uid) || setresuid(uid, uid, uid) ||
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) return 67;
    char *const args[] = {"/probe-automation-http", uid == 5001 ? "allow" : "deny", NULL};
    char *const env[] = {"HOME=/tmp", "PATH=/plat", NULL};
    execve(args[0], args, env); return 68;
}
