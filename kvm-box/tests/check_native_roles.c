#define _GNU_SOURCE
#include "../guest/native/role-policy.h"
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>

static int failures;
static void deny(const char *name, long result) {
    if (result != -1 || errno != EPERM) { fprintf(stderr, "%s not denied: %ld/%d\n", name, result, errno); ++failures; }
}
static void *thread_main(void *arg) { *(int *)arg = 7; return NULL; }
static int after_exec(uid_t wanted) {
    struct __user_cap_header_struct h = { .version = _LINUX_CAPABILITY_VERSION_3 };
    struct __user_cap_data_struct d[2];
    uid_t real, effective, saved;
    if (getresuid(&real, &effective, &saved) || real != wanted || effective != wanted || saved != wanted ||
        prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 1 || prctl(PR_GET_SECCOMP) != 2 ||
        syscall(SYS_capget, &h, d)) return 71;
    for (int i = 0; i < 2; ++i) if (d[i].effective || d[i].permitted || d[i].inheritable) return 72;
    for (int cap = 0; cap <= CAP_LAST_CAP; ++cap) if (prctl(PR_CAPBSET_READ, cap) != 0) return 73;
    gid_t groups[4]; int count = getgroups(4, groups);
    gid_t expected_gid = wanted == 1001 ? 1000 : wanted;
    gid_t extra_group = wanted == 1001 ? 1001 : 1000;
    if (getgid() != expected_gid || getegid() != expected_gid ||
        ((wanted == 1001 || wanted == 1003) ? count != 1 || groups[0] != extra_group : count != 0)) return 74;
    deny("setuid root", setuid(0));
    deny("setgid root", setgid(0));
    deny("vsock", socket(AF_VSOCK, SOCK_STREAM, 0));
    deny("packet capability", socket(AF_PACKET, SOCK_RAW, 0));
    deny("namespace", unshare(CLONE_NEWUSER));
    deny("setns", syscall(SYS_setns, -1, 0));
    deny("io_uring", syscall(SYS_io_uring_setup, 1, NULL));
    deny("ptrace", syscall(SYS_ptrace, 0, 0, 0, 0));
    deny("pidfd_getfd", syscall(SYS_pidfd_getfd, -1, 0, 0));
    deny("bpf", syscall(SYS_bpf, 0, NULL, 0));
    errno = 0;
    if (syscall(SYS_clone3, NULL, 0) != -1 || errno != ENOSYS) ++failures;
    int inet = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0), pair[2];
    if (inet < 0 || socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair)) return 75;
    close(inet); close(pair[0]); close(pair[1]);
    int value = 0; pthread_t thread;
    if (pthread_create(&thread, NULL, thread_main, &value) || pthread_join(thread, NULL) || value != 7) return 76;
    unsigned char *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED) return 77;
    memcpy(code, "\xb8\x2a\x00\x00\x00\xc3", 6);
    if (mprotect(code, 4096, PROT_READ | PROT_EXEC) || ((int (*)(void))code)() != 42) return 78;
    munmap(code, 4096);
    printf("role %u: setuid exec stayed unprivileged; syscall gates, threads and JIT passed\n", wanted);
    return failures ? 1 : 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "after-exec")) return after_exec((uid_t)atoi(argv[2]));
    char line[128]; unsigned inside, outside, count;
    FILE *map = fopen("/proc/self/uid_map", "r");
    if (argc != 2 || getuid() || getpid() != 1 || !map || !fgets(line, sizeof line, map) ||
        sscanf(line, "%u %u %u", &inside, &outside, &count) != 3 || inside || !outside || count != 1) return 64;
    fclose(map);
    if (native_enter_role(0) != -1 || errno != EINVAL || getuid()) return 65;
    int executable = open(argv[1], O_RDONLY | O_CLOEXEC);
    if (executable < 0) return 66;
    char statement[160];
    if (native_role_statement(statement, sizeof statement) < 0) return 67;
    fputs(statement, stdout); fflush(stdout);
    for (uid_t uid = 1000; uid <= 1003; ++uid) {
        pid_t child = fork();
        if (child == 0) {
            char id[16]; snprintf(id, sizeof id, "%u", uid);
            if (native_enter_role(uid)) { perror("role drop"); _exit(68); }
            char *const args[] = { "role-setuid-fixture", "after-exec", id, NULL };
            char *const env[] = { NULL };
            fexecve(executable, args, env);
            _exit(69);
        }
        int status = 0;
        if (child < 0 || waitpid(child, &status, 0) != child || status) {
            fprintf(stderr, "role %u failed: wait status %d\n", uid, status); return 1;
        }
    }
    close(executable);
    return 0;
}
