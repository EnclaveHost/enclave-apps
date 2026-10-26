//
// x11-auth.h — just enough Xauthority for the raw X11 backend.
//
// One job: find the MIT-MAGIC-COOKIE-1 for a local display in the file named
// by XAUTHORITY, and refuse anything about that file that is not exactly what
// a private, single-user authority file looks like. No Xlib or libXau, no
// heap, no output: the caller decides what to say, and never says the cookie.
//
// x_auth_load() returns
//    1  cookie[] holds the 16-byte cookie for this display
//    0  XAUTHORITY is not in the environment at all — the legacy
//       unauthenticated path; cookie[] is left untouched
//   -1  anything else: XAUTHORITY set (even empty) but the path is not
//       absolute, the file is missing, unsafe, truncated, malformed, over a
//       limit, holds conflicting cookies or none for this display; cookie[]
//       is zeroed
//
// Only XAUTHORITY is consulted. There is deliberately no $HOME/.Xauthority
// fallback and no way to turn a -1 into an unauthenticated connection.
//
// File format (libXau): a sequence of records, each a big-endian 16-bit
// family followed by four fields — address, display number, protocol name,
// protocol data — every one a big-endian 16-bit length and that many bytes.
//

#ifndef X11_AUTH_H
#define X11_AUTH_H

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#ifndef HOST_NAME_MAX
#define HOST_NAME_MAX 255
#endif

#define X_AUTH_FILE_MAX     (64 * 1024)
#define X_AUTH_RECORDS_MAX  64
#define X_AUTH_FIELD_MAX    4096
#define X_AUTH_FAMILY_LOCAL 256
#define X_AUTH_FAMILY_WILD  65535
#define X_AUTH_COOKIE_LEN   16

static const char x_auth_mit[] = "MIT-MAGIC-COOKIE-1";

// Volatile stores, so wiping a buffer that is about to die is not optimized
// away as a dead store.
static void x_auth_clear(void *p, size_t n)
{
    volatile unsigned char *v = p;

    while (n--)
        *v++ = 0;
}

// Invariant for both readers: *off <= len on entry and on return.
static int x_auth_u16(const unsigned char *buf, size_t len, size_t *off,
                      unsigned *v)
{
    if (len - *off < 2)
        return -1;
    *v = ((unsigned)buf[*off] << 8) | buf[*off + 1];
    *off += 2;
    return 0;
}

static int x_auth_field(const unsigned char *buf, size_t len, size_t *off,
                        const unsigned char **p, unsigned *n)
{
    if (x_auth_u16(buf, len, off, n) < 0 || *n > X_AUTH_FIELD_MAX
        || len - *off < *n)
        return -1;
    *p = buf + *off;
    *off += *n;
    return 0;
}

// The canonical spelling of a display number, which is the only one matched.
static size_t x_auth_decimal(char out[6], unsigned v)
{
    char tmp[5];
    size_t n = 0, i;

    do {
        tmp[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v && n < sizeof(tmp));
    for (i = 0; i < n; i++)
        out[i] = tmp[n - 1 - i];
    return n;
}

// Walk every record, whether or not it matches: one malformed record anywhere
// fails the whole file. A record matches when it is FamilyLocal for this host
// (or FamilyWild, as xauth writes for some setups), names this display, and is
// MIT-MAGIC-COOKIE-1. A matching record whose cookie is not 16 bytes, or two
// matching records with different cookies, is an error rather than a choice.
static int x_auth_parse(const unsigned char *buf, size_t len, int display,
                        unsigned char cookie[X_AUTH_COOKIE_LEN])
{
    char host[HOST_NAME_MAX + 1];
    char num[6];
    unsigned char found[X_AUTH_COOKIE_LEN];
    size_t hlen, nlen, off = 0;
    int records = 0, have = 0, ret = -1;

    if (gethostname(host, sizeof(host)) < 0)
        return -1;
    host[sizeof(host) - 1] = '\0';
    hlen = strlen(host);
    nlen = x_auth_decimal(num, (unsigned)display);

    while (off < len) {
        unsigned family, alen, dlen, mlen, clen;
        const unsigned char *addr, *dnum, *name, *data;
        int match;

        if (++records > X_AUTH_RECORDS_MAX
            || x_auth_u16(buf, len, &off, &family) < 0
            || x_auth_field(buf, len, &off, &addr, &alen) < 0
            || x_auth_field(buf, len, &off, &dnum, &dlen) < 0
            || x_auth_field(buf, len, &off, &name, &mlen) < 0
            || x_auth_field(buf, len, &off, &data, &clen) < 0)
            goto out;

        match = (family == X_AUTH_FAMILY_WILD
                 || (family == X_AUTH_FAMILY_LOCAL && hlen && alen == hlen
                     && !memcmp(addr, host, hlen)))
            && dlen == nlen && !memcmp(dnum, num, nlen)
            && mlen == sizeof(x_auth_mit) - 1
            && !memcmp(name, x_auth_mit, mlen);
        if (!match)
            continue;
        if (clen != X_AUTH_COOKIE_LEN
            || (have && memcmp(found, data, X_AUTH_COOKIE_LEN)))
            goto out;
        memcpy(found, data, X_AUTH_COOKIE_LEN);
        have = 1;
    }
    if (have) {
        memcpy(cookie, found, X_AUTH_COOKIE_LEN);
        ret = 1;
    }
out:
    x_auth_clear(found, sizeof(found));
    return ret;
}

static int x_auth_load(int display, unsigned char cookie[X_AUTH_COOKIE_LEN])
{
    const char *path = getenv("XAUTHORITY");
    unsigned char buf[X_AUTH_FILE_MAX + 1];   // one spare byte detects growth
    struct stat st;
    size_t total = 0;
    ssize_t k;
    int fd, ret = -1;

    if (!path)
        return 0;
    x_auth_clear(cookie, X_AUTH_COOKIE_LEN);
    if (path[0] != '/' || display < 0 || display > 65535)
        return -1;

    // O_NONBLOCK so a FIFO planted at the path cannot hang the open; the
    // S_ISREG check below then rejects it.
    fd = open(path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NOCTTY | O_NONBLOCK);
    if (fd < 0)
        return -1;
    if (fstat(fd, &st) < 0 || !S_ISREG(st.st_mode) || st.st_uid != getuid()
        || (st.st_mode & 07777) != 0600 || st.st_nlink != 1
        || st.st_size <= 0 || st.st_size > X_AUTH_FILE_MAX) {
        close(fd);
        return -1;
    }
    for (;;) {
        k = read(fd, buf + total, sizeof(buf) - total);
        if (k < 0 && errno == EINTR)
            continue;
        if (k <= 0 || (total += (size_t)k) == sizeof(buf))
            break;
    }
    close(fd);

    // Clean EOF, and exactly the size fstat promised: a file that changed
    // under the read is not trusted.
    if (k == 0 && total == (size_t)st.st_size)
        ret = x_auth_parse(buf, total, display, cookie);
    x_auth_clear(buf, total);
    if (ret != 1)
        x_auth_clear(cookie, X_AUTH_COOKIE_LEN);
    return ret;
}

#endif
