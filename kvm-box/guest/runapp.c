/* Starts the controller's runtime the way the platform's own init starts an app's (isolation/m2/dominit.c in the
 * platform repository): unprivileged and under the platform's seccomp filter, with the filter's statement left
 * where the front reads it into every attestation. The platform's front counts a process as the runtime only when
 * it is not root, requires each such process to be under a filter, and a judge refuses a document that covers no
 * runtime process: a runtime started as root from the start script is none.
 *
 * usage: runapp <program> [arguments...]. This process stays, as root, until the runtime ends, and returns its
 * status. The filter is app-seccomp.h of the platform revision the image is built against (-I at build time). */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include "app-seccomp.h"

#define APP_UID 1000
#define APP_GID 1000
#define SECCOMP_DIR "/run/enclave"
#define SECCOMP_STATEMENT SECCOMP_DIR "/seccomp"

/* dominit.c's drop: the bounding set, the ambient set, the groups, the gid, the uid, the capability sets, then
 * no_new_privs, and every part checked back. -> NULL, or the step that failed or did not hold. */
static const char *drop_to_app(void) {
    for (int c = 0; c < 64; c++)
        if (prctl(PR_CAPBSET_DROP, c, 0, 0, 0) != 0) { if (errno == EINVAL) break; return "PR_CAPBSET_DROP"; }
    if (prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) != 0 && errno != EINVAL) return "PR_CAP_AMBIENT_CLEAR_ALL";
    if (setgroups(0, NULL) != 0) return "setgroups";
    if (setresgid(APP_GID, APP_GID, APP_GID) != 0) return "setresgid";
    if (setresuid(APP_UID, APP_UID, APP_UID) != 0) return "setresuid";
    struct { uint32_t version; int pid; } h = { 0x20080522, 0 };
    struct { uint32_t effective, permitted, inheritable; } caps[2];
    memset(caps, 0, sizeof caps);
    if (syscall(SYS_capset, &h, caps) != 0) return "capset";
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) return "PR_SET_NO_NEW_PRIVS";

    uid_t ru, eu, su;
    gid_t rg, eg, sg;
    if (getresuid(&ru, &eu, &su) != 0 || ru != APP_UID || eu != APP_UID || su != APP_UID) return "the uid did not hold";
    if (getresgid(&rg, &eg, &sg) != 0 || rg != APP_GID || eg != APP_GID || sg != APP_GID) return "the gid did not hold";
    if (getgroups(0, NULL) != 0) return "a supplementary group remains";
    if (setuid(0) == 0 || setgid(0) == 0) return "uid or gid 0 could be taken back";
    memset(caps, 0xff, sizeof caps);
    h.version = 0x20080522;
    h.pid = 0;
    if (syscall(SYS_capget, &h, caps) != 0) return "capget";
    for (int i = 0; i < 2; i++)
        if (caps[i].effective || caps[i].permitted || caps[i].inheritable) return "a capability remains";
    for (int c = 0; c < 64; c++) {
        int b = prctl(PR_CAPBSET_READ, c, 0, 0, 0);
        if (b < 0) break;
        if (b) return "the bounding set is not empty";
    }
    if (prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 1) return "no_new_privs is not set";
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    int statement[2];
    if (pipe2(statement, O_CLOEXEC) != 0) { perror("KVMBOX runapp: pipe"); return 1; }
    pid_t child = fork();
    if (child < 0) { perror("KVMBOX runapp: fork"); return 1; }
    if (child == 0) {
        close(statement[0]);
        const char *bad = drop_to_app();
        if (bad) { dprintf(2, "KVMBOX runtime not started: the drop failed at %s\n", bad); _exit(126); }
        if (app_seccomp_install() != 0) { dprintf(2, "KVMBOX runtime not started: seccomp filter: %s\n", strerror(errno)); _exit(126); }
        char st[160];
        int n = app_seccomp_statement(st, sizeof st);
        if (n <= 0 || write(statement[1], st, (size_t)n) != n) { dprintf(2, "KVMBOX runtime not started: no seccomp statement\n"); _exit(126); }
        execv(argv[1], argv + 1);   /* closes the statement pipe: nothing the runtime runs can write it */
        dprintf(2, "KVMBOX runtime not started: %s\n", strerror(errno));
        _exit(127);
    }
    close(statement[1]);
    char st[256];
    size_t got = 0;
    for (;;) {
        ssize_t r = read(statement[0], st + got, sizeof st - 1 - got);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) break;
        got += (size_t)r;
        if (got == sizeof st - 1) break;
    }
    close(statement[0]);
    if (got && st[got - 1] == '\n') {
        mkdir(SECCOMP_DIR, 0700);
        int w = open(SECCOMP_STATEMENT ".tmp", O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW, 0600);
        if (w < 0 || write(w, st, got) != (ssize_t)got || close(w) != 0 || rename(SECCOMP_STATEMENT ".tmp", SECCOMP_STATEMENT) != 0)
            dprintf(2, "KVMBOX runapp: could not record the seccomp statement: %s\n", strerror(errno));
        else
            dprintf(2, "KVMBOX runtime: unprivileged and filtered (%.*s)\n", (int)got - 1, st);
    } else {
        dprintf(2, "KVMBOX runapp: the runtime gave no seccomp statement\n");
    }
    int status = 0;
    while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
    return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}
