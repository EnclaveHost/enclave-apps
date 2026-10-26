/* Called only in a fresh, single-threaded nsinit child before a fixed exec.
 * The caller closes inherited descriptors, supplies a clean explicit env and
 * directs stdio to null/private logs. This does not create a filesystem/network
 * sandbox or perform runtime admission; those are mandatory outer boundaries.
 */
#ifndef KVMBOX_NATIVE_ROLE_POLICY_H
#define KVMBOX_NATIVE_ROLE_POLICY_H
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <grp.h>
#include <linux/capability.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>
#include "wx-policy.h"

static inline int native_role_statement(char *out, size_t length) {
    return native_seccomp_statement(out, length);
}

static int native_enter_role(uid_t uid) {
    if (uid < 1000 || uid > 1003) { errno = EINVAL; return -1; }
    if (getuid() || geteuid()) { errno = EPERM; return -1; }
    int end = 0;
    for (int cap = 0; cap < 64; ++cap) {
        if (prctl(PR_CAPBSET_READ, cap, 0, 0, 0) < 0) {
            if (errno != EINVAL) return -1;
            end = 1; break;
        }
        if (prctl(PR_CAPBSET_DROP, cap, 0, 0, 0)) return -1;
    }
    if (!end) { errno = EOVERFLOW; return -1; }
    if (prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) ||
        prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0)) return -1;
    /* Preserve the service's desktop-facing socket group (1000); the current
     * service creates that socket with its primary GID. Its sole supplementary
     * group 1001 reaches the worker broker. The inner UI has group 1000 only.
     */
    gid_t gid = uid == 1001 ? 1000 : uid;
    gid_t extra_group = uid == 1001 ? 1001 : 1000;
    if ((uid == 1001 || uid == 1003) ? setgroups(1, &extra_group) : setgroups(0, NULL)) return -1;
    if (setresgid(gid, gid, gid) || setresuid(uid, uid, uid)) return -1;
    struct __user_cap_header_struct h = { .version = _LINUX_CAPABILITY_VERSION_3, .pid = 0 };
    struct __user_cap_data_struct empty[2] = {{0}, {0}}, actual[2];
    if (syscall(SYS_capset, &h, empty) || syscall(SYS_capget, &h, actual)) return -1;
    for (int i = 0; i < 2; ++i)
        if (actual[i].effective || actual[i].permitted || actual[i].inheritable) { errno = EPERM; return -1; }
    uid_t real, effective, saved;
    gid_t greal, geffective, gsaved;
    if (getresuid(&real, &effective, &saved) || getresgid(&greal, &geffective, &gsaved) ||
        real != uid || effective != uid || saved != uid ||
        greal != gid || geffective != gid || gsaved != gid || setuid(0) == 0) { errno = EPERM; return -1; }
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || native_seccomp_install()) return -1;
    return 0;
}
#endif
