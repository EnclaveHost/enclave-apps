/* Fixed default-home seeding for the measured native desktop nsinit.
 *
 * Runs once as UID 0, PID 1, after native_prepare_root() has chrooted into the
 * prepared root and BEFORE any role or service starts. It takes no arguments,
 * environment, configuration, caller path or tenant data, reads no source
 * files, and copies exactly the immutable tables compiled in from
 * home-defaults.h into the fresh, empty /home/desktop tmpfs. Paths are
 * resolved with openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS) from a pinned "/",
 * and with RESOLVE_NO_XDEV as well once the home mount root is pinned.
 *
 * Returns 0 only after every directory and file is created, written and given
 * exact ownership/mode; otherwise -1 with errno. On failure partial contents
 * are left as they are (nothing is deleted) and the caller must abort PID 1 /
 * the namespace: no role may start and no readiness byte may be sent. A call
 * on a nonempty home refuses (ENOTEMPTY); it never merges or resets.
 *
 * fd 3 (readiness) and fd 4 (supervisor pidfd) are never touched. Only
 * descriptors opened here are closed, on every return path. umask, IDs and
 * capabilities are unchanged. No dynamic allocation or recursion is used.
 * Preconditions and remaining obligations: home-seed-notes.md.
 */
#ifndef KVMBOX_NATIVE_HOME_SEED_H
#define KVMBOX_NATIVE_HOME_SEED_H
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <fcntl.h>
#include <linux/magic.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <unistd.h>
#include "home-defaults.h"

#if !defined(__x86_64__)
#error Native desktop home seeding requires x86_64
#endif
#ifndef SYS_openat2
#define SYS_openat2 437
#endif
#ifndef STATX_ATTR_MOUNT_ROOT
#define STATX_ATTR_MOUNT_ROOT 0x00002000
#endif

#define HS_KERNEL "6.6.87.2-enclave-kvm-box"
#define HS_DESKTOP 1000
#define HS_MAX_FILES 128
#define HS_MAX_DIRS 64
#define HS_MAX_TOTAL ((size_t)1 << 20)
#define HS_MAX_PATH 255
#define HS_MAX_NAME 64
#define HS_MAX_DEPTH 8
/* Own copies of the openat2 ABI avoid libc/<linux/openat2.h> clashes. */
#define HS_NO_XDEV 0x01ULL
#define HS_NO_SYMLINKS 0x04ULL
#define HS_BENEATH 0x08ULL
#define HS_RES (HS_BENEATH | HS_NO_SYMLINKS)
#define HS_RESX (HS_RES | HS_NO_XDEV)
#define HS_NFILES (sizeof native_home_files / sizeof native_home_files[0])
#define HS_NDIRS (sizeof native_home_dirs / sizeof native_home_dirs[0])

/* Measured desktop defaults (home-defaults-manifest.json): path and byte count of every file. */
static const struct hs_expect { const char *path; size_t size; } hs_expected[] = {
    { ".bash_logout", 220 }, { ".bashrc", 3526 }, { ".blank.xbm", 160 },
    { ".config/gtk-3.0/settings.ini", 62 }, { ".fluxbox/apps", 74 },
    { ".fluxbox/init", 581 }, { ".fluxbox/keys", 4872 }, { ".fluxbox/menu", 213 },
    { ".fluxbox/overlay", 89 }, { ".mozilla/firefox/kvm-box/chrome/userContent.css", 829 },
    { ".mozilla/firefox/kvm-box/user.js", 3083 }, { ".mozilla/firefox/profiles.ini", 93 },
    { ".profile", 807 }, { ".themes/RiscBox/gtk-3.0/gtk.css", 964 },
};
#define HS_EXPECT_FILES 14
#define HS_EXPECT_BYTES ((size_t)15573)
_Static_assert(sizeof hs_expected / sizeof hs_expected[0] == HS_EXPECT_FILES, "expected table");
_Static_assert(HS_NFILES >= 1 && HS_NFILES <= HS_MAX_FILES, "home file table size");
_Static_assert(HS_NDIRS >= 1 && HS_NDIRS <= HS_MAX_DIRS, "home directory table size");

struct hs_open_how { unsigned long long flags, mode, resolve; };
struct hs_dirent { uint64_t ino; int64_t off; unsigned short reclen; unsigned char type; char name[]; };

static int hs_fail(int e) { errno = e; return -1; }
static void hs_close(int *fd) { if (*fd >= 0) close(*fd); *fd = -1; }
static int hs_at(int base, const char *rel, int flags, mode_t mode, unsigned long long resolve) {
    struct hs_open_how how = { (unsigned long long)(flags | O_CLOEXEC), mode, resolve };
    return (int)syscall(SYS_openat2, base, rel, &how, sizeof how);
}

/* Relative path of 1..8 components of [A-Za-z0-9._-], none empty, "." or "..". */
static int hs_path_ok(const char *p) {
    size_t len, n = 0;
    int depth = 0;
    if (!p || (len = strnlen(p, HS_MAX_PATH + 1)) == 0 || len > HS_MAX_PATH) return 0;
    for (size_t i = 0; i <= len; ++i) {
        char c = p[i];
        if (c == '/' || c == 0) {
            if (n == 0 || n > HS_MAX_NAME || ++depth > HS_MAX_DEPTH) return 0;
            if (p[i - 1] == '.' && (n == 1 || (n == 2 && p[i - 2] == '.'))) return 0;
            n = 0;
        } else if ((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
                   c == '.' || c == '_' || c == '-') {
            ++n;
        } else {
            return 0;
        }
    }
    return 1;
}
static size_t hs_parent_len(const char *p) {
    const char *s = strrchr(p, '/');
    return s ? (size_t)(s - p) : 0;
}
/* Whether the first len bytes of p name one of native_home_dirs[0..limit). */
static int hs_is_dir(const char *p, size_t len, size_t limit) {
    for (size_t j = 0; j < limit; ++j)
        if (strlen(native_home_dirs[j]) == len && !memcmp(native_home_dirs[j], p, len)) return 1;
    return 0;
}

/* Fixed tables: bounded valid paths, parent-before-child directories that are
 * each an ancestor of some file, unique files equal to the measured set. */
static int hs_table_ok(void) {
    size_t total = 0;
    if (HS_NFILES != HS_EXPECT_FILES || HS_NFILES > HS_MAX_FILES || HS_NDIRS > HS_MAX_DIRS) return hs_fail(EINVAL);
    for (size_t i = 0; i < HS_NDIRS; ++i) {
        const char *d = native_home_dirs[i];
        size_t len, pl;
        int used = 0;
        if (!hs_path_ok(d)) return hs_fail(EINVAL);
        len = strlen(d);
        pl = hs_parent_len(d);
        if ((pl && !hs_is_dir(d, pl, i)) || hs_is_dir(d, len, i)) return hs_fail(EINVAL);
        for (size_t j = 0; j < HS_NFILES && !used; ++j) {
            const char *p = native_home_files[j].path;
            used = p && !strncmp(p, d, len) && p[len] == '/';
        }
        if (!used) return hs_fail(EINVAL);
    }
    for (size_t i = 0; i < HS_NFILES; ++i) {
        const struct native_home_file *f = &native_home_files[i];
        size_t k = 0, pl;
        if (!hs_path_ok(f->path) || !f->data || f->size > HS_MAX_TOTAL - total ||
            (f->mode != 0644 && f->mode != (S_IFREG | 0644))) return hs_fail(EINVAL);
        total += f->size;
        pl = hs_parent_len(f->path);
        if ((pl && !hs_is_dir(f->path, pl, HS_NDIRS)) || hs_is_dir(f->path, strlen(f->path), HS_NDIRS))
            return hs_fail(EINVAL);
        for (size_t j = 0; j < i; ++j)
            if (!strcmp(native_home_files[j].path, f->path)) return hs_fail(EINVAL);
        while (k < HS_EXPECT_FILES && strcmp(hs_expected[k].path, f->path)) ++k;
        if (k == HS_EXPECT_FILES || hs_expected[k].size != f->size) return hs_fail(EINVAL);
    }
    return total == HS_EXPECT_BYTES ? 0 : hs_fail(EINVAL);
}

/* 0 if the directory has no entries besides "." and ".."; no allocation. */
static int hs_empty(int fd) {
    _Alignas(8) char buf[1024];
    for (;;) {
        long n = syscall(SYS_getdents64, fd, buf, sizeof buf);
        if (n < 0) return -1;
        if (n == 0) return 0;
        for (long o = 0; o < n;) {
            const struct hs_dirent *d = (const struct hs_dirent *)(buf + o);
            if (d->reclen == 0) return hs_fail(EIO);
            if (strcmp(d->name, ".") && strcmp(d->name, "..")) return hs_fail(ENOTEMPTY);
            o += d->reclen;
        }
    }
}

/* Real /home/desktop: desktop 0700, empty writable nosuid,nodev tmpfs mount root. */
static int hs_home(int root, struct stat *st) {
    struct statx sx;
    struct statfs sf;
    int e, fd = hs_at(root, "home/desktop", O_RDONLY | O_DIRECTORY | O_NOFOLLOW, 0, HS_RES);
    if (fd < 0) return -1;
    if (fstat(fd, st) || statx(fd, "", AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW, STATX_BASIC_STATS, &sx) ||
        fstatfs(fd, &sf)) goto fail;
    errno = EPERM;
    if (!S_ISDIR(st->st_mode) || st->st_uid != HS_DESKTOP || st->st_gid != HS_DESKTOP ||
        (st->st_mode & 07777) != 0700 || !(sx.stx_attributes_mask & STATX_ATTR_MOUNT_ROOT) ||
        !(sx.stx_attributes & STATX_ATTR_MOUNT_ROOT) || sf.f_type != TMPFS_MAGIC ||
        (sf.f_flags & (ST_RDONLY | ST_NOSUID | ST_NODEV)) != (ST_NOSUID | ST_NODEV)) goto fail;
    if (hs_empty(fd)) goto fail;
    return fd;
fail:
    e = errno;
    close(fd);
    return hs_fail(e);
}

/* Pinned parent directory of rel beneath home ("." for top level); *leaf is its last component. */
static int hs_parent(int home, const char *rel, const char **leaf) {
    char buf[HS_MAX_PATH + 1];
    size_t pl = hs_parent_len(rel);
    if (pl) memcpy(buf, rel, pl); else buf[pl++] = '.';
    buf[pl] = 0;
    *leaf = strrchr(rel, '/') ? strrchr(rel, '/') + 1 : rel;
    return hs_at(home, buf, O_PATH | O_DIRECTORY, 0, HS_RESX);
}

/* Freshly created root inode on the home filesystem with one name, then exact
 * desktop ownership and mode; size < 0 skips the size check. */
static int hs_own(int fd, dev_t dev, mode_t type, nlink_t links, mode_t mode, off_t size) {
    struct stat st;
    if (fstat(fd, &st)) return -1;
    if ((st.st_mode & S_IFMT) != type || st.st_dev != dev || st.st_nlink != links ||
        st.st_uid != 0 || st.st_gid != 0 || (size >= 0 && st.st_size != size)) return hs_fail(EPERM);
    if (fchown(fd, HS_DESKTOP, HS_DESKTOP) || fchmod(fd, mode) || fstat(fd, &st)) return -1;
    if (st.st_uid != HS_DESKTOP || st.st_gid != HS_DESKTOP || (st.st_mode & 07777) != mode ||
        st.st_nlink != links) return hs_fail(EPERM);
    return 0;
}

static int hs_write(int fd, const unsigned char *p, size_t n) {
    while (n) {
        ssize_t w = write(fd, p, n);
        if (w < 0 && errno == EINTR) continue;
        if (w < 0) return -1;
        if (w == 0) return hs_fail(EIO);
        p += w;
        n -= (size_t)w;
    }
    return 0;
}

static int native_seed_home(void) {
    int root = -1, home = -1, parent = -1, fd = -1, e;
    const char *leaf;
    struct stat home_st, st;
    struct utsname u;

    if (getuid() || geteuid() || getpid() != 1) return hs_fail(EPERM);
    if (uname(&u)) return -1;
    if (strcmp(u.release, HS_KERNEL)) return hs_fail(EPERM);
    /* Whole table is checked before the first mutation. */
    if (hs_table_ok()) return -1;
    if ((root = open("/", O_PATH | O_DIRECTORY | O_CLOEXEC)) < 0) return -1;
    if ((home = hs_home(root, &home_st)) < 0) goto fail;

    for (size_t i = 0; i < HS_NDIRS; ++i) {
        if ((parent = hs_parent(home, native_home_dirs[i], &leaf)) < 0 || mkdirat(parent, leaf, 0700) ||
            (fd = hs_at(parent, leaf, O_RDONLY | O_DIRECTORY | O_NOFOLLOW, 0, HS_RESX)) < 0 ||
            hs_own(fd, home_st.st_dev, S_IFDIR, 2, 0755, -1)) goto fail;
        hs_close(&fd);
        hs_close(&parent);
    }
    for (size_t i = 0; i < HS_NFILES; ++i) {
        const struct native_home_file *f = &native_home_files[i];
        if ((parent = hs_parent(home, f->path, &leaf)) < 0 ||
            (fd = hs_at(parent, leaf, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0600, HS_RESX)) < 0 ||
            hs_write(fd, f->data, f->size) ||
            hs_own(fd, home_st.st_dev, S_IFREG, 1, 0644, (off_t)f->size)) goto fail;
        e = close(fd);
        fd = -1;
        if (e) goto fail;
        hs_close(&parent);
    }

    /* The home root itself is never modified. */
    if (fstat(home, &st)) goto fail;
    if (st.st_dev != home_st.st_dev || st.st_ino != home_st.st_ino || st.st_uid != HS_DESKTOP ||
        st.st_gid != HS_DESKTOP || (st.st_mode & 07777) != 0700) { errno = EPERM; goto fail; }
    hs_close(&home);
    hs_close(&root);
    return 0;

fail:
    e = errno ? errno : EPERM;
    hs_close(&fd);
    hs_close(&parent);
    hs_close(&home);
    hs_close(&root);
    errno = e;
    return -1;
}
#endif
