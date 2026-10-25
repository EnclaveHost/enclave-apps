/* Fixed filesystem preparation for the measured native desktop nsinit.
 *
 * Runs once inside the supervisor's new PID/mount/IPC/UTS namespace as UID 0,
 * PID 1, before any role is launched. It takes no arguments, environment,
 * configuration or tenant path. Every path is a compile-time literal resolved
 * with openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS) from a pinned directory,
 * and every mount names pinned source/target descriptors through the original
 * /proc/self/fd path, which stays unchanged until chroot in this private namespace. Returns 0 only after chroot into the
 * prepared root and chdir("/"); otherwise -1 with errno, and PID 1 must exit.
 *
 * fd 3 (readiness) and fd 4 (supervisor pidfd) are never touched and no
 * readiness byte is written: home seeding and all services must follow.
 * Descriptors opened here are O_CLOEXEC and closed on every return path.
 * Preconditions and remaining obligations: native-root-notes.md.
 */
#ifndef KVMBOX_NATIVE_ROOT_H
#define KVMBOX_NATIVE_ROOT_H
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/magic.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error Native desktop root preparation requires x86_64
#endif
#ifndef SYS_openat2
#define SYS_openat2 437
#endif
#ifndef STATX_ATTR_MOUNT_ROOT
#define STATX_ATTR_MOUNT_ROOT 0x00002000
#endif

#define NR_KERNEL "6.6.87.2-enclave-kvm-box"
#define NR_DESKTOP 1000
#define NR_SHIELD 1001
#define NR_GATEWAY 1002
/* Own copies of the openat2 ABI avoid libc/<linux/openat2.h> clashes. */
#define NR_NO_XDEV 0x01ULL
#define NR_NO_SYMLINKS 0x04ULL
#define NR_BENEATH 0x08ULL
#define NR_RES (NR_BENEATH | NR_NO_SYMLINKS)
#define NR_ANY_GID ((gid_t)-1)
#define NR_SAFE 0 /* directory mode: not group/world writable */
#define NR_LOCKED (MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC)

struct nr_open_how { unsigned long long flags, mode, resolve; };

static const struct nr_device {
    const char *name;
    unsigned major, minor;
    mode_t mode;
    int desktop; /* re-owned to the desktop UID; exclusive guest only */
} nr_devices[] = {
    { "null", 1, 3, 0666, 0 }, { "zero", 1, 5, 0666, 0 },
    { "random", 1, 8, 0666, 0 }, { "urandom", 1, 9, 0666, 0 },
    { "full", 1, 7, 0666, 0 }, { "tty", 5, 0, 0666, 0 },
    { "uinput", 10, 223, 0600, 1 }, { "uhid", 10, 239, 0600, 1 },
};
#define NR_NDEV ((int)(sizeof nr_devices / sizeof nr_devices[0]))

enum { MP_PROC, MP_SYS, MP_DEV, MP_RUN, MP_TMP, MP_VARLOG, MP_VARTMP, MP_HOME, MP_MODELS, MP_COUNT };
static const char *const nr_mountpoints[MP_COUNT] = {
    "proc", "sys", "dev", "run", "tmp", "var/log", "var/tmp", "home/desktop", "models"
};

enum {
    NR_CWD, NR_ROOT, NR_ND, NR_ROOTFS, NR_MODELS, NR_SOCK, NR_SYS, NR_HOSTDEV, NR_INPUT,
    NR_DEV0, NR_TARGET = NR_DEV0 + NR_NDEV, NR_NEW, NR_MP0, NR_T0 = NR_MP0 + MP_COUNT, NR_T1, NR_T2, NR_FDS
};
_Static_assert(NR_TARGET - NR_DEV0 == NR_NDEV, "descriptor table must match nr_devices");

static int nr_fail(int e) { errno = e; return -1; }
static void nr_close(int *fd) { if (*fd >= 0) close(*fd); *fd = -1; }
static const char *nr_path(char buf[32], int fd) {
    snprintf(buf, 32, "/proc/self/fd/%d", fd);
    return buf;
}
static int nr_at(int base, const char *rel, int flags, unsigned long long resolve) {
    struct nr_open_how how = { .flags = (unsigned long long)(flags | O_CLOEXEC), .resolve = resolve };
    return (int)syscall(SYS_openat2, base, rel, &how, sizeof how);
}
/* 0 if fd is (want=1) or is not (want=0) the root of its mount. */
static int nr_mount_root(int fd, int want) {
    struct statx sx;
    if (statx(fd, "", AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW, STATX_BASIC_STATS, &sx)) return -1;
    if (!(sx.stx_attributes_mask & STATX_ATTR_MOUNT_ROOT)) return nr_fail(ENOTSUP);
    return !!(sx.stx_attributes & STATX_ATTR_MOUNT_ROOT) == want ? 0 : nr_fail(EPERM);
}
/* Exact effective ro/nosuid/nodev/noexec; MS_* and ST_* share these bits. */
static int nr_fs_ok(int fd, unsigned long flags, long magic) {
    struct statfs s;
    if (fstatfs(fd, &s)) return -1;
    if ((s.f_flags & 0xf) != (flags & 0xf) || (magic && s.f_type != magic)) return nr_fail(EPERM);
    return 0;
}
static int nr_source_mount(int fd, int readonly, long magic) {
    struct statfs s;
    if (nr_mount_root(fd, 1) || fstatfs(fd, &s)) return -1;
    if ((readonly && !(s.f_flags & ST_RDONLY)) || (magic && s.f_type != magic)) return nr_fail(EPERM);
    return 0;
}
static int nr_same(int a, int b) {
    struct stat x, y;
    if (fstat(a, &x) || fstat(b, &y)) return -1;
    if (x.st_dev != y.st_dev || x.st_ino != y.st_ino || x.st_rdev != y.st_rdev ||
        (x.st_mode & S_IFMT) != (y.st_mode & S_IFMT)) return nr_fail(EPERM);
    return 0;
}
/* Root-owned directory, one fixed relative path, no symlink components. */
static int nr_dir(int base, const char *rel, gid_t gid, mode_t mode) {
    struct stat st;
    int fd = nr_at(base, rel, O_PATH | O_DIRECTORY, NR_RES);
    if (fd < 0) return -1;
    if (fstat(fd, &st)) { int e = errno; close(fd); return nr_fail(e); }
    if (st.st_uid != 0 || (gid != NR_ANY_GID && st.st_gid != gid) ||
        (mode ? (st.st_mode & 07777) != mode : (st.st_mode & 022) != 0)) { close(fd); return nr_fail(EPERM); }
    return fd;
}
static int nr_empty(int dirfd) {
    int fd = openat(dirfd, ".", O_RDONLY | O_DIRECTORY | O_CLOEXEC), r = 0;
    DIR *d = fd < 0 ? NULL : fdopendir(fd);
    if (!d) { int e = errno; if (fd >= 0) close(fd); return nr_fail(e); }
    struct dirent *de;
    errno = 0;
    while ((de = readdir(d)))
        if (strcmp(de->d_name, ".") && strcmp(de->d_name, "..")) { errno = ENOTEMPTY; break; }
    if (de || errno) r = -1;
    int e = errno;
    closedir(d);
    errno = e;
    return r;
}
/* One fixed new directory in a fresh tmpfs; an existing entry is an error. */
static int nr_mkdir(int base, const char *name, mode_t mode, uid_t uid, gid_t gid) {
    if (mkdirat(base, name, 0700)) return -1;
    int fd = nr_at(base, name, O_RDONLY | O_DIRECTORY, NR_RES);
    if (fd < 0) return -1;
    if (fchown(fd, uid, gid) || fchmod(fd, mode)) { int e = errno; close(fd); return nr_fail(e); }
    return fd;
}
/* Regular placeholder for a non-directory bind; no mknod is needed. */
static int nr_file(int base, const char *name) {
    return openat(base, name, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600);
}
/* Non-recursive bind of a pinned source onto a pinned mountpoint, then exact
 * per-mount flags, verified on the new mount root resolved beneath base. */
static int nr_bind(int src, int mp, int base, const char *rel, unsigned long flags, long magic) {
    char s[32], t[32];
    if (mount(nr_path(s, src), nr_path(t, mp), NULL, MS_BIND, NULL)) return -1;
    int fd = nr_at(base, rel, O_PATH, NR_RES);
    if (fd < 0) return -1;
    int r = mount(NULL, nr_path(t, fd), NULL, MS_REMOUNT | MS_BIND | flags, NULL) ||
            nr_mount_root(fd, 1) || nr_same(fd, src) || nr_fs_ok(fd, flags, magic) ? -1 : 0;
    int e = errno;
    close(fd);
    errno = e;
    return r;
}
/* New filesystem instance on a pinned mountpoint; returns its verified root. */
static int nr_fresh(const char *type, int mp, int base, const char *rel,
                    unsigned long flags, const char *data, long magic) {
    char t[32];
    if (mount(type, nr_path(t, mp), type, flags, data)) return -1;
    int fd = nr_at(base, rel, O_PATH | O_DIRECTORY, NR_RES);
    if (fd < 0) return -1;
    if (nr_mount_root(fd, 1) || nr_fs_ok(fd, flags, magic)) { int e = errno; close(fd); return nr_fail(e); }
    return fd;
}

static int native_prepare_root(void) {
    int f[NR_FDS], touched[NR_NDEV] = { 0 }, target_checked = 0, rooted = 0, e;
    struct stat st, parent, orig[NR_NDEV];
    struct utsname u;
    char a[32];
    for (int i = 0; i < NR_FDS; ++i) f[i] = -1;
#define NR_TRY(x) do { if ((x) < 0) goto fail; } while (0)
#define NR_FD(i, x) do { if ((f[i] = (x)) < 0) goto fail; } while (0)
#define NR_DENY(c) do { if (c) { errno = EPERM; goto fail; } } while (0)

    if (getuid() || geteuid() || getpid() != 1) return nr_fail(EPERM);
    if (uname(&u)) return -1;
    if (strcmp(u.release, NR_KERNEL)) return nr_fail(EPERM);
    NR_FD(NR_CWD, open(".", O_PATH | O_DIRECTORY | O_CLOEXEC));
    NR_FD(NR_ROOT, open("/", O_PATH | O_DIRECTORY | O_CLOEXEC));

    /* Pin every host source before anything is mounted. The original /proc
     * must be a real procfs mount because all mounts go through its fd links. */
    NR_FD(NR_T0, nr_dir(f[NR_ROOT], "proc", NR_ANY_GID, NR_SAFE));
    NR_TRY(nr_source_mount(f[NR_T0], 0, PROC_SUPER_MAGIC));
    /* The original procfs must belong to an ancestor PID namespace, and our
     * mount namespace must differ from its init. Check before changing any
     * mount propagation. The supervisor must not remount child /proc first. */
    char self_pid[32];
    ssize_t pid_len = readlink("/proc/self", self_pid, sizeof self_pid - 1);
    NR_TRY(pid_len);
    NR_DENY(pid_len == 0 || pid_len >= (ssize_t)sizeof self_pid - 1);
    self_pid[pid_len] = 0;
    for (ssize_t i = 0; i < pid_len; ++i) NR_DENY(self_pid[i] < '0' || self_pid[i] > '9');
    NR_DENY(!strcmp(self_pid, "1"));
    struct stat init_mnt, self_mnt;
    NR_TRY(stat("/proc/1/ns/mnt", &init_mnt));
    NR_TRY(stat("/proc/self/ns/mnt", &self_mnt));
    NR_DENY(init_mnt.st_dev == self_mnt.st_dev && init_mnt.st_ino == self_mnt.st_ino);
    NR_TRY(mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL));
    nr_close(&f[NR_T0]);
    NR_FD(NR_T0, nr_dir(f[NR_ROOT], "plat", NR_ANY_GID, NR_SAFE));
    NR_FD(NR_T1, nr_dir(f[NR_T0], "native-desktop", NR_ANY_GID, NR_SAFE));
    NR_FD(NR_ROOTFS, nr_dir(f[NR_T1], "rootfs", NR_ANY_GID, NR_SAFE));
    NR_TRY(nr_source_mount(f[NR_ROOTFS], 1, 0));
    nr_close(&f[NR_T0]); nr_close(&f[NR_T1]);
    NR_FD(NR_T0, nr_dir(f[NR_ROOT], "run", NR_ANY_GID, NR_SAFE));
    NR_FD(NR_ND, nr_dir(f[NR_T0], "native-desktop", NR_GATEWAY, 0750));
    nr_close(&f[NR_T0]);
    NR_FD(NR_MODELS, nr_dir(f[NR_ND], "models", NR_ANY_GID, NR_SAFE));
    NR_TRY(nr_source_mount(f[NR_MODELS], 1, 0));
    /* Metadata only: liveness and peer checks belong to the broker/client. */
    NR_FD(NR_SOCK, nr_at(f[NR_ND], "worker.sock", O_PATH, NR_RES));
    NR_TRY(fstat(f[NR_SOCK], &st));
    NR_DENY(!S_ISSOCK(st.st_mode) || st.st_uid != 0 || st.st_gid != NR_SHIELD || (st.st_mode & 07777) != 0660);
    NR_FD(NR_SYS, nr_dir(f[NR_ROOT], "sys", NR_ANY_GID, NR_SAFE));
    NR_TRY(nr_source_mount(f[NR_SYS], 0, SYSFS_MAGIC));
    NR_FD(NR_HOSTDEV, nr_dir(f[NR_ROOT], "dev", NR_ANY_GID, NR_SAFE));
    NR_TRY(nr_source_mount(f[NR_HOSTDEV], 0, TMPFS_MAGIC));
    /* Created by the launcher (root:root 0755) even before any uinput device. */
    NR_FD(NR_INPUT, nr_dir(f[NR_HOSTDEV], "input", 0, 0755));
    NR_TRY(nr_mount_root(f[NR_INPUT], 0));
    for (int i = 0; i < NR_NDEV; ++i) {
        const struct nr_device *d = &nr_devices[i];
        NR_FD(NR_DEV0 + i, nr_at(f[NR_HOSTDEV], d->name, O_PATH, NR_RES | NR_NO_XDEV));
        NR_TRY(fstat(f[NR_DEV0 + i], &orig[i]));
        st = orig[i];
        NR_DENY(!S_ISCHR(st.st_mode) || st.st_rdev != makedev(d->major, d->minor) ||
                (st.st_mode & 07777) != d->mode ||
                !((st.st_uid == 0 && st.st_gid == 0) ||
                  (d->desktop && st.st_uid == NR_DESKTOP && st.st_gid == NR_DESKTOP)));
    }

    /* Fixed target: absent, or an existing empty root 0700 real directory on
     * the parent filesystem that is not itself a mount point. */
    f[NR_TARGET] = nr_at(f[NR_ND], "root", O_PATH | O_DIRECTORY, NR_RES);
    if (f[NR_TARGET] < 0 && errno == ENOENT) {
        NR_TRY(mkdirat(f[NR_ND], "root", 0700));
        NR_FD(NR_TARGET, nr_at(f[NR_ND], "root", O_RDONLY | O_DIRECTORY, NR_RES));
        NR_TRY(fchmod(f[NR_TARGET], 0700));
    }
    NR_TRY(f[NR_TARGET]);
    NR_TRY(fstat(f[NR_TARGET], &st));
    NR_TRY(fstat(f[NR_ND], &parent));
    NR_DENY(st.st_uid != 0 || st.st_gid != 0 || (st.st_mode & 07777) != 0700 || st.st_dev != parent.st_dev);
    NR_TRY(nr_mount_root(f[NR_TARGET], 0));
    NR_TRY(nr_empty(f[NR_TARGET]));

    /* From here any mount on the target is ours and is detached on failure. */
    target_checked = 1;
    NR_TRY(nr_bind(f[NR_ROOTFS], f[NR_TARGET], f[NR_ND], "root", MS_RDONLY | MS_NOSUID | MS_NODEV, 0));
    NR_FD(NR_NEW, nr_at(f[NR_ND], "root", O_PATH | O_DIRECTORY, NR_RES));
    NR_TRY(nr_same(f[NR_NEW], f[NR_ROOTFS]));
    /* Validate every image mountpoint (real directories, no symlinks, no
     * nested mounts) before mounting anything inside the new root. */
    for (int i = 0; i < MP_COUNT; ++i)
        NR_FD(NR_MP0 + i, nr_at(f[NR_NEW], nr_mountpoints[i], O_PATH | O_DIRECTORY, NR_RES | NR_NO_XDEV));

    NR_FD(NR_T0, nr_fresh("proc", f[NR_MP0 + MP_PROC], f[NR_NEW], "proc",
                          MS_NOSUID | MS_NODEV | MS_NOEXEC, "hidepid=2", PROC_SUPER_MAGIC));
    /* UID-0 udev must not write sysctls, sysrq, PCI config or IRQ controls.
     * Some proc write handlers rely only on inode permissions, not caps. */
    static const char *const proc_readonly[] = { "sys", "sysrq-trigger", "bus", "irq" };
    for (size_t i = 0; i < sizeof proc_readonly / sizeof proc_readonly[0]; ++i) {
        NR_FD(NR_T1, nr_at(f[NR_T0], proc_readonly[i], O_PATH, NR_RES | NR_NO_XDEV));
        NR_TRY(nr_bind(f[NR_T1], f[NR_T1], f[NR_T0], proc_readonly[i], NR_LOCKED, PROC_SUPER_MAGIC));
        nr_close(&f[NR_T1]);
    }
    nr_close(&f[NR_T0]); nr_close(&f[NR_T1]);
    NR_TRY(nr_bind(f[NR_SYS], f[NR_MP0 + MP_SYS], f[NR_NEW], "sys", NR_LOCKED, SYSFS_MAGIC));

    /* Private /dev: fixed pinned nodes, guest input tree and private pts/shm. */
    NR_FD(NR_T0, nr_fresh("tmpfs", f[NR_MP0 + MP_DEV], f[NR_NEW], "dev",
                          MS_NOSUID | MS_NODEV | MS_NOEXEC, "size=8m,mode=0755,uid=0,gid=0", TMPFS_MAGIC));
    for (int i = 0; i < NR_NDEV; ++i) {
        NR_FD(NR_T1, nr_file(f[NR_T0], nr_devices[i].name));
        NR_TRY(nr_bind(f[NR_DEV0 + i], f[NR_T1], f[NR_T0], nr_devices[i].name, MS_RDONLY | MS_NOSUID | MS_NOEXEC, 0));
        nr_close(&f[NR_T1]);
    }
    NR_FD(NR_T1, nr_mkdir(f[NR_T0], "input", 0755, 0, 0));
    NR_TRY(nr_bind(f[NR_INPUT], f[NR_T1], f[NR_T0], "input", MS_NOSUID | MS_NOEXEC, 0));
    nr_close(&f[NR_T1]);
    NR_FD(NR_T1, nr_mkdir(f[NR_T0], "pts", 0755, 0, 0));
    NR_FD(NR_T2, nr_fresh("devpts", f[NR_T1], f[NR_T0], "pts", MS_NOSUID | MS_NOEXEC,
                          "newinstance,ptmxmode=0666,mode=0600,gid=1000", DEVPTS_SUPER_MAGIC));
    nr_close(&f[NR_T1]); nr_close(&f[NR_T2]);
    NR_FD(NR_T1, nr_mkdir(f[NR_T0], "shm", 0755, 0, 0));
    NR_FD(NR_T2, nr_fresh("tmpfs", f[NR_T1], f[NR_T0], "shm", MS_NOSUID | MS_NODEV | MS_NOEXEC,
                          "size=256m,mode=1777,uid=0,gid=0", TMPFS_MAGIC));
    nr_close(&f[NR_T1]); nr_close(&f[NR_T2]);
    static const char *const links[][2] = {
        { "pts/ptmx", "ptmx" }, { "/proc/self/fd", "fd" }, { "/proc/self/fd/0", "stdin" },
        { "/proc/self/fd/1", "stdout" }, { "/proc/self/fd/2", "stderr" },
    };
    for (size_t i = 0; i < sizeof links / sizeof links[0]; ++i)
        NR_TRY(symlinkat(links[i][0], f[NR_T0], links[i][1]));
    nr_close(&f[NR_T0]);

    /* Private /run; the broker socket was pinned above, before this hides it. */
    NR_FD(NR_T0, nr_fresh("tmpfs", f[NR_MP0 + MP_RUN], f[NR_NEW], "run",
                          MS_NOSUID | MS_NODEV | MS_NOEXEC, "size=64m,mode=0755,uid=0,gid=0", TMPFS_MAGIC));
    NR_FD(NR_T1, nr_mkdir(f[NR_T0], "user", 0755, 0, 0));
    NR_FD(NR_T2, nr_mkdir(f[NR_T1], "1000", 0700, NR_DESKTOP, NR_DESKTOP));
    nr_close(&f[NR_T1]); nr_close(&f[NR_T2]);
    NR_FD(NR_T1, nr_mkdir(f[NR_T0], "udev", 0755, 0, 0));
    nr_close(&f[NR_T1]);
    NR_FD(NR_T1, nr_mkdir(f[NR_T0], "native-automation", 0750, NR_DESKTOP, NR_DESKTOP));
    nr_close(&f[NR_T1]);
    NR_FD(NR_T1, nr_mkdir(f[NR_T0], "shield", 0750, NR_SHIELD, NR_DESKTOP));
    NR_FD(NR_T2, nr_mkdir(f[NR_T1], "jobs", 0700, NR_SHIELD, NR_SHIELD));
    nr_close(&f[NR_T1]); nr_close(&f[NR_T2]);
    NR_FD(NR_T1, nr_mkdir(f[NR_T0], "shield-worker", 0750, 0, NR_SHIELD));
    NR_FD(NR_T2, nr_file(f[NR_T1], "gpu0"));
    /* A read-only mount does not block connect(); inode stays root:1001 0660. */
    NR_TRY(nr_bind(f[NR_SOCK], f[NR_T2], f[NR_T1], "gpu0", NR_LOCKED, 0));
    nr_close(&f[NR_T0]); nr_close(&f[NR_T1]); nr_close(&f[NR_T2]);

    NR_FD(NR_T0, nr_fresh("tmpfs", f[NR_MP0 + MP_TMP], f[NR_NEW], "tmp",
                          MS_NOSUID | MS_NODEV, "size=256m,mode=1777,uid=0,gid=0", TMPFS_MAGIC));
    NR_FD(NR_T1, nr_mkdir(f[NR_T0], ".X11-unix", 01777, 0, 0));
    nr_close(&f[NR_T0]); nr_close(&f[NR_T1]);
    NR_FD(NR_T0, nr_fresh("tmpfs", f[NR_MP0 + MP_VARLOG], f[NR_NEW], "var/log",
                          MS_NOSUID | MS_NODEV | MS_NOEXEC, "size=32m,mode=0755,uid=0,gid=0", TMPFS_MAGIC));
    nr_close(&f[NR_T0]);
    NR_FD(NR_T0, nr_fresh("tmpfs", f[NR_MP0 + MP_VARTMP], f[NR_NEW], "var/tmp",
                          MS_NOSUID | MS_NODEV, "size=256m,mode=1777,uid=0,gid=0", TMPFS_MAGIC));
    nr_close(&f[NR_T0]);
    /* Empty and ephemeral: nsinit must seed measured defaults before readiness. */
    NR_FD(NR_T0, nr_fresh("tmpfs", f[NR_MP0 + MP_HOME], f[NR_NEW], "home/desktop",
                          MS_NOSUID | MS_NODEV, "size=512m,mode=0700,uid=1000,gid=1000", TMPFS_MAGIC));
    nr_close(&f[NR_T0]);
    NR_TRY(nr_bind(f[NR_MODELS], f[NR_MP0 + MP_MODELS], f[NR_NEW], "models", NR_LOCKED, 0));

    /* Last mutation before chroot: uinput/UHID nodes are shared with the
     * exclusive guest's devtmpfs, so the new owner is visible guest-wide. */
    for (int i = 0; i < NR_NDEV; ++i) {
        if (!nr_devices[i].desktop) continue;
        touched[i] = 1;
        NR_TRY(chmod(nr_path(a, f[NR_DEV0 + i]), 0600));
        NR_TRY(chown(a, NR_DESKTOP, NR_DESKTOP));
        NR_TRY(fstat(f[NR_DEV0 + i], &st));
        NR_DENY(st.st_uid != NR_DESKTOP || st.st_gid != NR_DESKTOP || (st.st_mode & 07777) != 0600);
    }

    NR_TRY(fchdir(f[NR_NEW]));
    NR_TRY(chroot("."));
    rooted = 1;
    NR_TRY(chdir("/"));
    for (int i = 0; i < NR_FDS; ++i) nr_close(&f[i]);
    return 0;

fail:
    e = errno ? errno : EPERM;
    if (!rooted) {
        for (int i = 0; i < NR_NDEV; ++i)
            if (touched[i]) {
                if (chown(nr_path(a, f[NR_DEV0 + i]), orig[i].st_uid, orig[i].st_gid)) {}
                if (chmod(a, orig[i].st_mode & 07777)) {}
            }
        if (f[NR_CWD] >= 0 && fchdir(f[NR_CWD])) {}
        if (target_checked) {
            /* The target was verified not to be a mount point, so a mount
             * there now is this helper's; MNT_DETACH also drops its children. */
            int t = nr_at(f[NR_ND], "root", O_PATH | O_DIRECTORY, NR_RES);
            if (t >= 0 && nr_mount_root(t, 1) == 0) (void)umount2(nr_path(a, t), MNT_DETACH);
            if (t >= 0) close(t);
        }
    }
    for (int i = 0; i < NR_FDS; ++i) nr_close(&f[i]);
    errno = e;
    return -1;
#undef NR_TRY
#undef NR_FD
#undef NR_DENY
}
#endif
