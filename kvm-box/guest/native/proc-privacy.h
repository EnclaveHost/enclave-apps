/* Reject visible ancestor procfs views that expose other roles' argv.
 * The outer launcher must install these mounts before any untrusted process.
 * Higher ancestor namespaces outside this domain remain trusted platform TCB.
 */
#ifndef KVMBOX_PROC_PRIVACY_H
#define KVMBOX_PROC_PRIVACY_H
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <unistd.h>
#include <linux/magic.h>

static int np_options_private(char *options) {
    char *save = NULL;
    int hidden = 0;
    for (char *p = strtok_r(options, ",", &save); p; p = strtok_r(NULL, ",", &save)) {
        if (!strncmp(p, "hidepid=", 8)) {
            if (hidden || (strcmp(p + 8, "2") && strcmp(p + 8, "invisible"))) return 0;
            hidden = 1;
        }
        /* Nonzero gid would let an untrusted role bypass hidepid. All fixed
         * non-root roles must also have no group 0 membership. */
        if (!strncmp(p, "gid=", 4) && strcmp(p + 4, "0")) return 0;
    }
    return hidden;
}

/* Consumes one mutable kernel mountinfo line, tracking the actual /proc mount
 * by statx mount ID. All visible procfs aliases must have the same protection. */
static int np_mount_line(char *line, uint64_t wanted, int *found) {
    char *fields[128], *save = NULL, *end;
    size_t n = 0, dash = 6;
    for (char *p = strtok_r(line, " \n", &save); p; p = strtok_r(NULL, " \n", &save)) {
        if (n == 128) return -1;
        fields[n++] = p;
    }
    if (n < 10) return -1;
    while (dash < n && strcmp(fields[dash], "-")) ++dash;
    if (dash + 4 != n) return -1;
    errno = 0;
    unsigned long long id = strtoull(fields[0], &end, 10);
    if (errno || !id || *end || fields[0][0] == '-' || fields[0][0] == '+') return -1;
    int proc = !strcmp(fields[dash + 1], "proc");
    if (proc && !np_options_private(fields[dash + 3])) return -1;
    if (id == wanted) {
        if (*found || !proc || strcmp(fields[3], "/") || strcmp(fields[4], "/proc")) return -1;
        *found = 1;
    }
    return 0;
}

static int native_ancestor_proc_private(int procfd) {
    struct statx sx;
    struct statfs fs;
    if (statx(procfd, "", AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW, STATX_MNT_ID, &sx)) return -1;
    if (!(sx.stx_mask & STATX_MNT_ID)) { errno = ENOTSUP; return -1; }
    int fd = openat(procfd, "self/mountinfo", O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) return -1;
    if (fstatfs(fd, &fs) || fs.f_type != PROC_SUPER_MAGIC) { close(fd); errno = EPERM; return -1; }
    FILE *f = fdopen(fd, "r");
    if (!f) { int e = errno; close(fd); errno = e; return -1; }
    char line[16384];
    size_t total = 0;
    int found = 0, bad = 0;
    while (fgets(line, sizeof line, f)) {
        size_t n = strlen(line);
        total += n;
        if (!n || line[n - 1] != '\n' || total > (1U << 20) || np_mount_line(line, sx.stx_mnt_id, &found)) {
            bad = 1; break;
        }
    }
    if (ferror(f) || !found) bad = 1;
    fclose(f);
    if (bad) { errno = EPERM; return -1; }
    return 0;
}
#endif
