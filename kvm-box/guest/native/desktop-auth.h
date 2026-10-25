/* Fixed local X11 authorization for the native desktop. Called by nsinit
 * after native_prepare_root, before any child starts. The cookie exists only
 * in the private runtime tmpfs, never in an image, environment value or log.
 * Xorg and all X clients run as UID1000 and receive only the fixed file path.
 */
#ifndef KVMBOX_DESKTOP_AUTH_H
#define KVMBOX_DESKTOP_AUTH_H
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <fcntl.h>
#include <linux/magic.h>
#include <stdint.h>
#include <string.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <unistd.h>

static int native_desktop_auth(void) {
    struct utsname u;
    struct stat st;
    struct statfs fs;
    struct { uint64_t flags, mode, resolve; } how = {
        .flags = O_RDONLY | O_DIRECTORY | O_CLOEXEC,
        .resolve = 0x04 /* RESOLVE_NO_SYMLINKS, absolute fixed path */
    };
    unsigned char cookie[16], record[64];
    int directory = -1, file = -1, saved = 0;
    size_t filled = 0, n = 0;
    if (getuid() || geteuid() || getpid() != 1 || uname(&u) ||
        strcmp(u.release,"6.6.87.2-enclave-kvm-box") || strcmp(u.nodename,"kvm-box")) {
        errno = EPERM; return -1;
    }
    directory = (int)syscall(SYS_openat2,AT_FDCWD,"/run/user/1000",&how,sizeof how);
    if (directory < 0) return -1;
    if (fstat(directory,&st) || fstatfs(directory,&fs)) goto fail;
    if (st.st_uid != 1000 || st.st_gid != 1000 || (st.st_mode & 07777) != 0700 ||
        fs.f_type != TMPFS_MAGIC || (fs.f_flags & 15) != 14) {
        errno = EPERM; goto fail;
    }
    while (filled < sizeof cookie) {
        ssize_t r = getrandom(cookie + filled,sizeof cookie - filled,0);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) { if (!r) errno = EIO; goto fail; }
        filled += (size_t)r;
    }
    /* Xauthority network-byte-order fields: FamilyLocal, hostname, display,
     * protocol and opaque random cookie. No wildcard family or remote display. */
#define XA_U16(v) do { record[n++] = (unsigned char)((v) >> 8); record[n++] = (unsigned char)(v); } while (0)
#define XA_TEXT(v) do { XA_U16(sizeof(v)-1); memcpy(record+n,v,sizeof(v)-1); n += sizeof(v)-1; } while (0)
    XA_U16(256);
    XA_TEXT("kvm-box");
    XA_TEXT("0");
    XA_TEXT("MIT-MAGIC-COOKIE-1");
    XA_U16(sizeof cookie);
    memcpy(record+n,cookie,sizeof cookie); n += sizeof cookie;
#undef XA_TEXT
#undef XA_U16
    file = openat(directory,"Xauthority",O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC,0600);
    if (file < 0) goto fail;
    filled = 0;
    while (filled < n) {
        ssize_t r = write(file,record+filled,n-filled);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) { if (!r) errno = EIO; goto fail; }
        filled += (size_t)r;
    }
    if (fchown(file,1000,1000) || fchmod(file,0600)) goto fail;
    if (close(file)) { file = -1; goto fail; }
    file = -1;
    goto done;
fail:
    saved = errno ? errno : EIO;
done:
    if (file >= 0) close(file);
    close(directory);
    for (size_t i=0;i<sizeof cookie;i++) ((volatile unsigned char *)cookie)[i] = 0;
    for (size_t i=0;i<sizeof record;i++) ((volatile unsigned char *)record)[i] = 0;
    if (saved) { errno = saved; return -1; }
    return 0;
}
#endif
