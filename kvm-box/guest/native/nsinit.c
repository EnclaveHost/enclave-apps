/* Fixed PID 1 for the measured native desktop namespace.
 *
 * Started only by supervisor.c through fexecve: UID/EUID 0, PID 1 of fresh
 * PID/mount/IPC/UTS namespaces, no arguments, null stdio, fd 3 = readiness
 * pipe write end, fd 4 = pidfd of the supervisor, no other descriptors. It
 * composes the tested root, home, X-auth and role helpers, starts a fixed
 * service graph whose long-running members are all direct monitored
 * children, probes readiness under one monotonic deadline, writes exactly "R"
 * and then supervises until stop, supervisor death or the first failure.
 *
 * There is no shell, configuration file, tenant command, endpoint, path or
 * environment override, and no alternate or degraded desktop. Diagnostics on
 * stderr (null under the supervisor) carry only a fixed stage name, errno and
 * a wait status; never argv, environment, credentials or service output.
 * Obligations outside this file: nsinit.md.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/magic.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "native-root.h"
#include "home-seed.h"
#include "desktop-auth.h"
#include "role-policy.h"
#include "proc-privacy.h"

#if !defined(__x86_64__)
#error Native desktop nsinit requires x86_64
#endif

#define NI_KERNEL "6.6.87.2-enclave-kvm-box"
#define READY_FD 3
#define PARENT_FD 4
#define STARTUP_MS 115000     /* supervisor allows 120000 from before our exec */
#define EXEC_MS 5000          /* fork to confirmed execve */
#define TICK_MS 100
#define RETRY_MS 250
#define CREDS_MS 20000
#define PACTL_MS 5000
#define XPROBE_MS 3000
#define PROBE_MS 8000         /* readiness.py: interpreter start plus its own <=6 s bound */
#define CONNECT_MS 1000
#define TERM_GRACE_MS 2000    /* both graces together stay under the supervisor's 5 s kill */
#define KILL_GRACE_MS 1000
#define MAX_CHILDREN 24
#define DESKTOP_UID 1000
#define SERVICE_UID 1001
#define GATEWAY_UID 1002
#define UI_UID 1003
#define READINESS "/usr/local/libexec/kvm-box/readiness.py"
#define RUNTIME "/run/user/1000"
/* Own copies of the openat2 ABI, as in the helpers. */
#define NI_NO_XDEV 0x01ULL
#define NI_NO_SYMLINKS 0x04ULL
#define NI_BENEATH 0x08ULL

struct ni_how { unsigned long long flags, mode, resolve; };

/* One fixed process. uid 0 is used only by the udev helper, which applies
 * its own capability/seccomp policy; every other entry calls native_enter_role. */
struct launch {
    const char *name;
    uid_t uid;
    char *const *argv;
    char *const *envp;
    const char *dir;
    int daemon; /* critical long-running service: any exit fails the session */
};

struct child {
    pid_t pid;
    const char *name;
    int daemon, exited, status, exec_errno;
};

static char *const desktop_env[] = {
    "PATH=/usr/local/bin:/usr/bin:/bin", "HOME=/home/desktop", "LANG=C.UTF-8", "DISPLAY=:0",
    "XDG_RUNTIME_DIR=" RUNTIME, "XAUTHORITY=" RUNTIME "/Xauthority",
    "DBUS_SESSION_BUS_ADDRESS=unix:path=" RUNTIME "/bus",
    "PULSE_SERVER=unix:" RUNTIME "/pulse/native",
#ifdef KVMBOX_SHIELD_GRAPHICS
    /* Separate measured graphics image only. The device policy still denies
     * physical GPU nodes; these select the standard API implementation. */
    "VK_DRIVER_FILES=/opt/shield/mesa/share/vulkan/icd.d/lvp_icd.x86_64.json",
    "VK_ICD_FILENAMES=/opt/shield/mesa/share/vulkan/icd.d/lvp_icd.x86_64.json",
    "LIBGL_DRIVERS_PATH=/opt/shield/mesa/lib/dri",
    "LD_LIBRARY_PATH=/opt/shield/mesa/lib", "ZINK_DESCRIPTORS=lazy",
    "LIBGL_ALWAYS_SOFTWARE=1", "GALLIUM_DRIVER=zink",
    "MESA_LOADER_DRIVER_OVERRIDE=zink", "LIBGL_KOPPER_DRI2=true",
    "LP_NUM_THREADS=2",
#endif
    NULL
};
static char *const role_env[] = { "PATH=/usr/bin:/bin", "LANG=C.UTF-8", "HOME=/nonexistent", NULL };
static char *const no_env[] = { NULL };

static char *const udev_argv[] = { "/usr/local/libexec/kvm-box/udev-launch", NULL };
static char *const xorg_argv[] = {
    "/usr/lib/xorg/Xorg", ":0", "-config", "/xorg.conf", "-logfile", "/tmp/xorg.log", "-noreset",
    "-nolisten", "tcp", "-nolisten", "local", "-novtswitch", "-sharevts", "-auth", RUNTIME "/Xauthority", NULL
};
static char *const xprobe_argv[] = { "/usr/bin/xdpyinfo", "-display", ":0", NULL };
static char *const dbus_argv[] = {
    "/usr/bin/dbus-daemon", "--session", "--nofork", "--nopidfile", "--address=unix:path=" RUNTIME "/bus", NULL
};
static char *const dbus_probe_argv[] = {
    "/usr/bin/dbus-send", "--session", "--dest=org.freedesktop.DBus", "--type=method_call", "--print-reply",
    "/org/freedesktop/DBus", "org.freedesktop.DBus.ListNames", NULL
};
static char *const pulse_argv[] = { "/usr/bin/pulseaudio", "--daemonize=no", "--exit-idle-time=-1", NULL };
static char *const sink_argv[] = {
    "/usr/bin/pactl", "load-module", "module-null-sink", "sink_name=kvm_box",
    "sink_properties=device.description=KVM_Box", NULL
};
static char *const default_sink_argv[] = { "/usr/bin/pactl", "set-default-sink", "kvm_box", NULL };
static char *const fluxbox_argv[] = { "/usr/bin/fluxbox", NULL };
static char *const sunshine_argv[] = { "/usr/bin/sunshine", "/sunshine.conf", NULL };
static char *const x11vnc_argv[] = {
    "/usr/bin/x11vnc", "-display", ":0", "-auth", RUNTIME "/Xauthority", "-localhost", "-forever",
    "-shared", "-nopw", "-rfbport", "5900", "-quiet", NULL
};
static char *const websockify_argv[] = { "/usr/bin/websockify", "127.0.0.1:6080", "127.0.0.1:5900", NULL };
static char *const service_argv[] = {
    "/usr/bin/python3", "-I", "/shield/service.py", "--config", "/shield/native-service.json", NULL
};
static char *const automation_argv[] = { "/usr/local/libexec/kvm-box/automation", NULL };
static char *const automation_probe_argv[] = { "/usr/bin/python3", "-I", READINESS, "automation", NULL };
static char *const inner_argv[] = { "/usr/local/libexec/kvm-box/inner", NULL };
static char *const worker_probe_argv[] = { "/usr/bin/python3", "-I", READINESS, "worker", NULL };
static char *const compute_probe_argv[] = { "/usr/bin/python3", "-I", READINESS, "compute", NULL };
static char *const inner_probe_argv[] = { "/usr/bin/python3", "-I", READINESS, "inner", NULL };

#define DESKTOP(name, argv, daemon) { name, DESKTOP_UID, argv, desktop_env, "/home/desktop", daemon }
#define ROLE(name, uid, argv, daemon) { name, uid, argv, role_env, "/", daemon }
static const struct launch L_UDEV = { "udev", 0, udev_argv, no_env, "/", 1 };
static const struct launch L_XORG = DESKTOP("xorg", xorg_argv, 1);
static const struct launch L_XPROBE = DESKTOP("xorg-probe", xprobe_argv, 0);
static const struct launch L_DBUS = DESKTOP("dbus", dbus_argv, 1);
static const struct launch L_DBUS_PROBE = DESKTOP("dbus-probe", dbus_probe_argv, 0);
static const struct launch L_PULSE = DESKTOP("pulseaudio", pulse_argv, 1);
static const struct launch L_SINK = DESKTOP("pulse-sink", sink_argv, 0);
static const struct launch L_DEFAULT_SINK = DESKTOP("pulse-default-sink", default_sink_argv, 0);
static const struct launch L_FLUXBOX = DESKTOP("fluxbox", fluxbox_argv, 1);
static const struct launch L_SUNSHINE = DESKTOP("sunshine", sunshine_argv, 1);
static const struct launch L_X11VNC = DESKTOP("x11vnc", x11vnc_argv, 1);
static const struct launch L_WEBSOCKIFY = ROLE("websockify", UI_UID, websockify_argv, 1);
static const struct launch L_SERVICE = ROLE("shield-service", SERVICE_UID, service_argv, 1);
static const struct launch L_AUTOMATION = DESKTOP("automation", automation_argv, 1);
static const struct launch L_AUTOMATION_PROBE = ROLE("automation-probe", UI_UID, automation_probe_argv, 0);
static const struct launch L_INNER = ROLE("inner", UI_UID, inner_argv, 1);
static const struct launch L_WORKER_PROBE = ROLE("worker-probe", SERVICE_UID, worker_probe_argv, 0);
static const struct launch L_COMPUTE_PROBE = ROLE("compute-probe", UI_UID, compute_probe_argv, 0);
static const struct launch L_INNER_PROBE = ROLE("inner-probe", GATEWAY_UID, inner_probe_argv, 0);

/* Sunshine UDP ports are bound only once a stream starts and are not checked. */
static const struct { unsigned short port; const char *name; } tcp_ports[] = {
    { 5900, "tcp-5900" }, { 6080, "tcp-6080" }, { 47984, "tcp-47984" },
    { 47989, "tcp-47989" }, { 47990, "tcp-47990" }, { 48010, "tcp-48010" },
};

static struct child kids[MAX_CHILDREN];
static int sigfd = -1;
static int ready_open = 1;      /* fd 3 not yet written/closed */
static int namespace_ready;     /* kill(-1, ...) permitted only once set */
static int running;             /* "R" sent; the startup deadline no longer applies */
static int tearing_down;
static int stop_requested;
static int64_t deadline;
static const char *stage = "entry";
static const char *fail_stage;
static int fail_errno, fail_status = -1, oneshot_status = -1;

static int64_t now_ms(void) {
    struct timespec t;
    /* CLOCK_MONOTONIC cannot fail here; if it did, exiting PID 1 closes fd 3
     * without "R" and the kernel kills every namespace descendant. */
    if (clock_gettime(CLOCK_MONOTONIC, &t)) _exit(111);
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

/* Records the first failure only; always returns -1. */
static int fail(const char *where, int err) {
    if (!fail_stage && !tearing_down) {
        fail_stage = where;
        fail_errno = err ? err : EIO;
    }
    return -1;
}

static void diag(const char *what, int err, int status) {
    char line[96];
    int n = snprintf(line, sizeof line, "nsinit: stage=%s errno=%d status=%d\n", what, err, status);
    if (n > 0 && write(2, line, (size_t)n < sizeof line ? (size_t)n : sizeof line - 1) < 0) {}
}

static int ni_at(int base, const char *rel, int flags, unsigned long long resolve) {
    struct ni_how how = { (unsigned long long)(flags | O_CLOEXEC), 0, resolve };
    return (int)syscall(SYS_openat2, base, rel, &how, sizeof how);
}

static int write_all(int fd, const char *p, size_t n) {
    while (n) {
        ssize_t w = write(fd, p, n);
        if (w < 0 && errno == EINTR) continue;
        if (w < 0) return -1;
        if (w == 0) { errno = EIO; return -1; }
        p += w;
        n -= (size_t)w;
    }
    return 0;
}

static struct child *find_child(pid_t pid) {
    for (int i = 0; i < MAX_CHILDREN; ++i)
        if (kids[i].pid == pid) return &kids[i];
    return NULL;
}

/* Reaps every exited descendant: tracked children and reparented orphans.
 * Each is first observed with WNOWAIT, so a one-shot's private process group
 * is swept while its zombie still pins that PID/PGID against reuse. */
static void reap(void) {
    for (;;) {
        siginfo_t si;
        int st = 0;
        pid_t got;
        memset(&si, 0, sizeof si);
        if (waitid(P_ALL, 0, &si, WEXITED | WNOHANG | WNOWAIT | __WALL)) {
            if (errno == EINTR) continue;
            return; /* ECHILD: nothing left */
        }
        if (si.si_pid <= 0) return;
        struct child *k = find_child(si.si_pid);
        if (k && !k->daemon) (void)kill(-si.si_pid, SIGKILL);
        while ((got = waitpid(si.si_pid, &st, __WALL)) < 0 && errno == EINTR) {}
        if (got != si.si_pid) return;
        if (!k) continue;
        k->exited = 1;
        k->status = st;
        if (k->daemon && !fail_stage && !tearing_down) {
            fail_status = st;
            fail(k->name, ECHILD);
        }
    }
}

static int children_remain(void) {
    siginfo_t si;
    memset(&si, 0, sizeof si);
    return !(waitid(P_ALL, 0, &si, WEXITED | WNOHANG | WNOWAIT | __WALL) < 0 && errno == ECHILD);
}

static void drain_signals(void) {
    struct signalfd_siginfo si;
    for (;;) {
        ssize_t n = read(sigfd, &si, sizeof si);
        if (n < 0 && errno == EINTR) continue;
        if (n != (ssize_t)sizeof si) return;
        if (si.ssi_signo != SIGTERM && si.ssi_signo != SIGINT) continue; /* SIGCHLD: reap() follows */
        /* The supervisor is outside this PID namespace, so its signals carry
         * sender PID 0 here. A namespace process asking PID 1 to stop is a fault. */
        if (si.ssi_pid == 0) stop_requested = 1;
        else fail("signal", EPERM);
    }
}

static int session_over(void) { return stop_requested || fail_stage != NULL; }

/* One bounded event-loop step: sleeps until `until` (clamped to the startup
 * deadline before readiness), a signal, supervisor death or readiness of `fd`.
 * Always drains signals and reaps. Returns 1 when fd is ready, 0 otherwise and
 * -1 once the session must end (stop, parent death, failure or deadline). */
static int pump(int64_t until, int fd, short events) {
    struct pollfd p[3] = {
        { .fd = sigfd, .events = POLLIN }, { .fd = PARENT_FD, .events = POLLIN }, { .fd = fd, .events = events },
    };
    int64_t now, left;
    int n, timeout;
    if (session_over()) return -1;
    now = now_ms();
    if (!running) {
        if (now >= deadline) return fail(stage, ETIMEDOUT);
        if (until > deadline) until = deadline;
    }
    left = until == INT64_MAX ? -1 : until <= now ? 0 : until - now;
    timeout = left < 0 ? -1 : left > INT_MAX ? INT_MAX : (int)left;
    n = poll(p, 3, timeout); /* a negative fd entry is ignored by poll */
    if (n < 0 && errno != EINTR) return fail("poll", errno);
    if (n > 0 && p[0].revents) drain_signals();
    reap();
    /* A pidfd becomes readable when the supervisor exits. */
    if (n > 0 && p[1].revents) fail("supervisor", EPIPE);
    if (session_over()) return -1;
    return n > 0 && fd >= 0 && p[2].revents ? 1 : 0;
}

static int checkpoint(const char *name) {
    stage = name;
    return pump(0, -1, 0) < 0 ? -1 : 0;
}

/* Runs in the fresh single-threaded child; never returns. */
static void child_exec(const struct launch *l, int errw, char *secret, size_t secret_len) {
    struct sigaction dfl;
    struct stat st;
    sigset_t none;
    int e, nullfd;
    memset(&dfl, 0, sizeof dfl);
    dfl.sa_handler = SIG_DFL;
    sigemptyset(&dfl.sa_mask);
    /* PID 1 ignores SIGPIPE and blocks TERM/INT/CHLD; none of that is inherited. */
    for (int s = 1; s < NSIG; ++s)
        if (s != SIGKILL && s != SIGSTOP) (void)sigaction(s, &dfl, NULL);
    errno = 0;
    sigemptyset(&none);
    if (sigprocmask(SIG_SETMASK, &none, NULL) || setsid() < 0) goto out;
    nullfd = open("/dev/null", O_RDWR | O_NOFOLLOW | O_CLOEXEC);
    if (nullfd < 0 || fstat(nullfd, &st)) goto out;
    if (!S_ISCHR(st.st_mode) || st.st_rdev != makedev(1, 3)) { errno = EPERM; goto out; }
    for (int i = 0; i < 3; ++i)
        if (dup2(nullfd, i) != i) goto out;
    /* Keep only stdio and the CLOEXEC exec-status pipe (always >= 3). This
     * drops fd 3 (readiness), fd 4 (supervisor pidfd), the signalfd and all
     * other PID 1 descriptors. */
    if ((errw > 3 && syscall(SYS_close_range, 3U, (unsigned)errw - 1, 0)) ||
        syscall(SYS_close_range, (unsigned)errw + 1, ~0U, 0)) goto out;
    umask(077);
    if (l->uid && native_enter_role(l->uid)) goto out;
    if (chdir(l->dir)) goto out;
    execve(l->argv[0], l->argv, l->envp);
out:
    e = errno ? errno : EPERM;
    if (secret) explicit_bzero(secret, secret_len);
    if (write_all(errw, (const char *)&e, sizeof e)) {}
    _exit(127);
}

/* Starts one fixed process and waits (bounded, observing signals, parent
 * death and the deadline) until execve is confirmed by EOF on the CLOEXEC
 * status pipe. Returns the child slot, or -1 after recording a failure. A
 * one-shot that could not exec is returned with exec_errno set so the caller
 * can treat it as "not ready"; a daemon that could not exec is fatal. */
static int spawn(const struct launch *l, char *secret, size_t secret_len) {
    int slot, p[2], err = 0;
    size_t got = 0;
    pid_t pid;
    int64_t until;
    for (slot = 0; slot < MAX_CHILDREN && kids[slot].pid; ++slot) {}
    if (slot == MAX_CHILDREN) return fail("children", ENOSPC);
    if (pipe2(p, O_CLOEXEC)) return fail(l->name, errno);
    pid = fork();
    if (pid == 0) {
        close(p[0]);
        child_exec(l, p[1], secret, secret_len);
    }
    err = errno;
    close(p[1]);
    if (pid < 0) { close(p[0]); return fail(l->name, err); }
    err = 0;
    /* Tracked as non-critical until exec is confirmed, so a pre-exec failure
     * is reported with its errno rather than as a generic daemon exit. */
    kids[slot] = (struct child){ .pid = pid, .name = l->name };
    until = now_ms() + EXEC_MS;
    for (;;) {
        int ready = pump(until, p[0], POLLIN);
        ssize_t n;
        if (ready < 0) { close(p[0]); return -1; }
        if (!ready) {
            if (now_ms() >= until) { close(p[0]); return fail(l->name, ETIMEDOUT); }
            continue;
        }
        n = read(p[0], (char *)&err + got, sizeof err - got);
        if (n < 0 && (errno == EINTR || errno == EAGAIN)) continue;
        if (n < 0) { err = errno; close(p[0]); return fail(l->name, err); }
        if (n == 0) break;
        got += (size_t)n;
        if (got == sizeof err) break;
    }
    close(p[0]);
    if (got) {
        if (got != sizeof err || !err) err = EIO;
        if (l->daemon) return fail(l->name, err);
        kids[slot].exec_errno = err;
        return slot;
    }
    if (l->daemon) {
        if (kids[slot].exited) {
            fail_status = kids[slot].status;
            return fail(l->name, ECHILD);
        }
        kids[slot].daemon = 1;
    }
    return slot;
}

/* Waits for a one-shot child. 0: exited 0. 1: failed, could not exec or timed
 * out (killed with its process group) - "not ready", never a daemon failure.
 * -1: the session is over. */
static int finish_oneshot(int slot, int64_t until) {
    struct child *k = &kids[slot];
    int r = 0;
    while (!k->exited) {
        if (!r && now_ms() >= until) {
            (void)kill(-k->pid, SIGKILL);
            (void)kill(k->pid, SIGKILL); /* not yet reaped: the PID is still ours */
            r = 1;
            until = INT64_MAX; /* still bounded by the startup deadline */
        }
        if (pump(until, -1, 0) < 0) return -1;
    }
    oneshot_status = k->status;
    if (k->exec_errno || !WIFEXITED(k->status) || WEXITSTATUS(k->status)) r = 1;
    *k = (struct child){ 0 };
    return r;
}

static int oneshot(const struct launch *l, int64_t limit_ms, char *secret, size_t secret_len) {
    int slot = spawn(l, secret, secret_len);
    return slot < 0 ? -1 : finish_oneshot(slot, now_ms() + limit_ms);
}

static int required_oneshot(const struct launch *l, int64_t limit_ms) {
    int r = oneshot(l, limit_ms, NULL, 0);
    if (r > 0) {
        fail_status = oneshot_status;
        return fail(l->name, ECHILD);
    }
    return r;
}

/* Bounded retries of a fixed probe; only the global deadline ends them. */
static int probe(const struct launch *l, int64_t attempt_ms) {
    for (;;) {
        int r = oneshot(l, attempt_ms, NULL, 0);
        if (r <= 0) return r;
        if (pump(now_ms() + RETRY_MS, -1, 0) < 0) return -1;
    }
}

static int start(const struct launch *l) { return spawn(l, NULL, 0) < 0 ? -1 : 0; }

/* A socket node of the expected owner at a fixed path. Readiness signal only:
 * the clients that use these sockets run as the owning UID. */
static int wait_socket(const char *path, uid_t uid) {
    for (;;) {
        struct stat st;
        if (!fstatat(AT_FDCWD, path, &st, AT_SYMLINK_NOFOLLOW) && S_ISSOCK(st.st_mode) && st.st_uid == uid)
            return 0;
        if (pump(now_ms() + TICK_MS, -1, 0) < 0) return -1;
    }
}

/* Fixed loopback TCP connect by root. The measured nft source-port rules
 * authenticate the responding service UID. 1: accepted, 0: not yet, -1: over. */
static int tcp_accepting(unsigned short port) {
    struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port),
                             .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    int r = 0, fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (fd < 0) return fail("tcp", errno);
    if (!connect(fd, (struct sockaddr *)&a, sizeof a)) {
        r = 1;
    } else if (errno == EINPROGRESS) {
        int64_t until = now_ms() + CONNECT_MS;
        for (;;) {
            int p = pump(until, fd, POLLOUT);
            if (p < 0) { close(fd); return -1; }
            if (p > 0) {
                int err = 0;
                socklen_t len = sizeof err;
                r = !getsockopt(fd, SOL_SOCKET, SO_ERROR, &err, &len) && err == 0;
                break;
            }
            if (now_ms() >= until) break;
        }
    }
    close(fd);
    return r;
}

static int wait_tcp(unsigned short port) {
    for (;;) {
        int r = tcp_accepting(port);
        if (r) return r < 0 ? -1 : 0;
        if (pump(now_ms() + RETRY_MS, -1, 0) < 0) return -1;
    }
}

/* The readiness helper must be the image's fixed root-owned read-only file. */
static int check_readiness_script(void) {
    struct stat st;
    if (lstat(READINESS, &st)) return fail("readiness-script", errno);
    if (!S_ISREG(st.st_mode) || st.st_uid != 0 || st.st_gid != 0 || (st.st_mode & 07222))
        return fail("readiness-script", EPERM);
    return 0;
}

/* Fresh per-boot Sunshine credential: 32 random bytes as 64 lowercase hex.
 * Stored only for the UI role (/run/native-ui, 1003:1003 0700/0600) and passed
 * once to Sunshine's own hashing as UID 1000, before any other UID 1000 process
 * exists. Never logged; PID 1 copies are wiped; no reuse or fallback. */
static int sunshine_credentials(void) {
    static const char digits[] = "0123456789abcdef";
    unsigned char raw[32];
    char hex[65];
    struct stat st, rst;
    struct statfs fs;
    struct statx sx;
    int root = -1, run = -1, dir = -1, file = -1, home = -1, state = -1, e = 0, r = -1;
    size_t got = 0;

    memset(hex, 0, sizeof hex);
    while (got < sizeof raw) {
        ssize_t n = getrandom(raw + got, sizeof raw - got, 0);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { e = n ? errno : EIO; goto out; }
        got += (size_t)n;
    }
    for (size_t i = 0; i < sizeof raw; ++i) {
        hex[2 * i] = digits[raw[i] >> 4];
        hex[2 * i + 1] = digits[raw[i] & 15];
    }
    hex[64] = 0;
    explicit_bzero(raw, sizeof raw);

    /* /run must be the root helper's private rw,nosuid,nodev,noexec tmpfs root. */
    if ((root = open("/", O_PATH | O_DIRECTORY | O_CLOEXEC)) < 0 ||
        (run = ni_at(root, "run", O_RDONLY | O_DIRECTORY, NI_BENEATH | NI_NO_SYMLINKS)) < 0 ||
        fstat(run, &rst) || fstatfs(run, &fs) ||
        statx(run, "", AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW, STATX_BASIC_STATS, &sx)) { e = errno; goto out; }
    if (!S_ISDIR(rst.st_mode) || rst.st_uid || rst.st_gid || (rst.st_mode & 07777) != 0755 ||
        fs.f_type != TMPFS_MAGIC || (fs.f_flags & 0xf) != (ST_NOSUID | ST_NODEV | ST_NOEXEC) ||
        !(sx.stx_attributes_mask & STATX_ATTR_MOUNT_ROOT) || !(sx.stx_attributes & STATX_ATTR_MOUNT_ROOT)) {
        e = EPERM; goto out;
    }
    /* Exclusive creation: any existing entry, symlink or earlier state fails. */
    if (mkdirat(run, "native-ui", 0700) ||
        (dir = ni_at(run, "native-ui", O_RDONLY | O_DIRECTORY | O_NOFOLLOW,
                     NI_BENEATH | NI_NO_SYMLINKS | NI_NO_XDEV)) < 0 ||
        fstat(dir, &st)) { e = errno; goto out; }
    if (!S_ISDIR(st.st_mode) || st.st_uid || st.st_gid || st.st_nlink != 2 || st.st_dev != rst.st_dev) {
        e = EPERM; goto out;
    }
    if (fchown(dir, UI_UID, UI_UID) || fchmod(dir, 0700) || fstat(dir, &st)) { e = errno; goto out; }
    if (st.st_uid != UI_UID || st.st_gid != UI_UID || (st.st_mode & 07777) != 0700) { e = EPERM; goto out; }
    file = openat(dir, "sunshine-password", O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600);
    if (file < 0 || fstat(file, &st)) { e = errno; goto out; }
    if (!S_ISREG(st.st_mode) || st.st_uid || st.st_nlink != 1 || st.st_size || st.st_dev != rst.st_dev) {
        e = EPERM; goto out;
    }
    if (write_all(file, hex, 64) || fchown(file, UI_UID, UI_UID) || fchmod(file, 0600) || fstat(file, &st)) {
        e = errno; goto out;
    }
    if (st.st_uid != UI_UID || st.st_gid != UI_UID || (st.st_mode & 07777) != 0600 ||
        st.st_size != 64 || st.st_nlink != 1) { e = EPERM; goto out; }
    {
        int c = close(file);
        file = -1;
        if (c) { e = errno; goto out; }
    }

    {
        char *argv[] = { "/usr/bin/sunshine", "/sunshine.conf", "--creds", "owner", hex, NULL };
        const struct launch creds = { "sunshine-creds", DESKTOP_UID, argv, desktop_env, "/home/desktop", 0 };
        int x = oneshot(&creds, CREDS_MS, hex, sizeof hex);
        explicit_bzero(hex, sizeof hex);
        if (x) {
            if (x > 0) { fail_status = oneshot_status; fail("sunshine-creds", ECHILD); }
            goto out;
        }
    }

    /* Sunshine's hashed state: a single-link regular file writable only by 1000. */
    if ((home = ni_at(root, "home/desktop", O_PATH | O_DIRECTORY, NI_BENEATH | NI_NO_SYMLINKS)) < 0 ||
        (state = ni_at(home, ".config/sunshine/sunshine_state.json", O_PATH,
                       NI_BENEATH | NI_NO_SYMLINKS | NI_NO_XDEV)) < 0 ||
        fstat(state, &st)) { e = errno; goto out; }
    if (!S_ISREG(st.st_mode) || st.st_uid != DESKTOP_UID || (st.st_mode & 07022) || st.st_nlink != 1 ||
        st.st_size <= 0) { e = EPERM; goto out; }
    r = 0;
out:
    explicit_bzero(raw, sizeof raw);
    explicit_bzero(hex, sizeof hex);
    if (state >= 0) close(state);
    if (home >= 0) close(home);
    if (file >= 0) close(file);
    if (dir >= 0) close(dir);
    if (run >= 0) close(run);
    if (root >= 0) close(root);
    if (r && e) return fail("credentials", e);
    return r;
}

static int signal_ready(void) {
    ssize_t n;
    for (int i = 0; i < MAX_CHILDREN; ++i)
        if (kids[i].pid && kids[i].daemon && kids[i].exited) return fail(kids[i].name, ECHILD);
    while ((n = write(READY_FD, "R", 1)) < 0 && errno == EINTR) {}
    if (n != 1) return fail("ready", n < 0 ? errno : EIO);
    ready_open = 0;
    running = 1;
    /* Linux releases the descriptor even when close reports an error. */
    if (close(READY_FD) && errno != EINTR) return fail("ready", errno);
    return 0;
}

#define RUN(name, call) do { if (checkpoint(name) || (call) < 0) return -1; } while (0)
#define SYS(name, call) do { if (checkpoint(name)) return -1; if ((call) < 0) return fail(name, errno); } while (0)

static int start_session(void) {
    SYS("root", native_prepare_root());
    /* PID 1, private PID/mount namespaces and the new root are verified. */
    namespace_ready = 1;
    SYS("hostname", sethostname("kvm-box", 7));
    SYS("home", native_seed_home());
    SYS("xauth", native_desktop_auth());
    RUN("credentials", sunshine_credentials());

    RUN("udev", start(&L_UDEV));
    RUN("udev-control", wait_socket("/run/udev/control", 0));
    RUN("readiness-script", check_readiness_script());
    RUN("worker-probe", probe(&L_WORKER_PROBE, PROBE_MS));
    /* Started early: it hashes model/policy/calibration before listening. */
    RUN("shield-service", start(&L_SERVICE));

    RUN("xorg", start(&L_XORG));
    RUN("xorg-probe", probe(&L_XPROBE, XPROBE_MS));
    RUN("dbus", start(&L_DBUS));
    RUN("dbus-socket", wait_socket(RUNTIME "/bus", DESKTOP_UID));
    RUN("dbus-probe", probe(&L_DBUS_PROBE, XPROBE_MS));
    RUN("pulseaudio", start(&L_PULSE));
    RUN("pulse-socket", wait_socket(RUNTIME "/pulse/native", DESKTOP_UID));
    RUN("pulse-sink", required_oneshot(&L_SINK, PACTL_MS));
    RUN("pulse-default-sink", required_oneshot(&L_DEFAULT_SINK, PACTL_MS));
    RUN("fluxbox", start(&L_FLUXBOX));
    RUN("sunshine", start(&L_SUNSHINE));
    RUN("x11vnc", start(&L_X11VNC));
    RUN("websockify", start(&L_WEBSOCKIFY));
    RUN("automation", start(&L_AUTOMATION));
    RUN("automation-socket", wait_socket("/run/native-automation/desktop.sock", DESKTOP_UID));
    RUN("inner", start(&L_INNER));

    for (size_t i = 0; i < sizeof tcp_ports / sizeof tcp_ports[0]; ++i)
        RUN(tcp_ports[i].name, wait_tcp(tcp_ports[i].port));
    RUN("compute-probe", probe(&L_COMPUTE_PROBE, PROBE_MS));
    RUN("inner-probe", probe(&L_INNER_PROBE, PROBE_MS));
    RUN("automation-probe", probe(&L_AUTOMATION_PROBE, PROBE_MS));
    RUN("ready", signal_ready());
    return 0;
}

static int setup_signals(void) {
    struct sigaction dfl, ign;
    sigset_t set;
    memset(&dfl, 0, sizeof dfl);
    memset(&ign, 0, sizeof ign);
    dfl.sa_handler = SIG_DFL;
    ign.sa_handler = SIG_IGN;
    sigemptyset(&dfl.sa_mask);
    sigemptyset(&ign.sa_mask);
    sigemptyset(&set);
    sigaddset(&set, SIGTERM);
    sigaddset(&set, SIGINT);
    sigaddset(&set, SIGCHLD);
    /* Block first: a stop request during the reset is queued, not lost. Blocked
     * signals are never discarded as "ignored" for a namespace init. */
    if (sigprocmask(SIG_SETMASK, &set, NULL)) return -1;
    for (int s = 1; s < NSIG; ++s)
        if (s != SIGKILL && s != SIGSTOP) (void)sigaction(s, &dfl, NULL);
    /* Plain SIG_DFL (never an inherited SIG_IGN or SA_NOCLDWAIT) keeps exit
     * statuses for waitpid. EPIPE from fd 3 is handled as an error. */
    if (sigaction(SIGCHLD, &dfl, NULL) || sigaction(SIGPIPE, &ign, NULL)) return -1;
    sigfd = signalfd(-1, &set, SFD_CLOEXEC | SFD_NONBLOCK);
    return sigfd < 0 ? -1 : 0;
}

/* fd 4: a pidfd for a live process, identified through the original
 * (ancestor) procfs, whose fdinfo reports a positive PID there. */
static int parent_pidfd_ok(void) {
    char buf[1024];
    size_t got = 0;
    const char *p;
    long pid = 0;
    int fd = open("/proc/self/fdinfo/4", O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) return -1;
    while (got < sizeof buf - 1) {
        ssize_t n = read(fd, buf + got, sizeof buf - 1 - got);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) { int e = errno; close(fd); errno = e; return -1; }
        if (n == 0) break;
        got += (size_t)n;
    }
    close(fd);
    buf[got] = 0;
    if (!(p = strstr(buf, "\nPid:\t"))) { errno = EPERM; return -1; }
    for (p += 6; *p >= '0' && *p <= '9' && pid < 1L << 30; ++p) pid = pid * 10 + (*p - '0');
    if (pid <= 0 || *p != '\n') { errno = EPERM; return -1; }
    return 0;
}

static int check_entry(void) {
    struct utsname u;
    struct stat st;
    struct statfs fs;
    struct pollfd parent = { .fd = PARENT_FD, .events = POLLIN };
    char link[64];
    ssize_t n;
    int fl, procfd, procroot;

    if (getuid() || geteuid() || getpid() != 1) return fail("entry", EPERM);
    if (uname(&u)) return fail("entry", errno);
    if (strcmp(u.release, NI_KERNEL)) return fail("entry", EPERM);
    for (int fd = 0; fd < 3; ++fd)
        if (fcntl(fd, F_GETFD) < 0) return fail("stdio", EBADF);
    /* Identify fds 3/4 through the original procfs, before the root helper
     * replaces /proc. */
    procfd = open("/proc/self/fd", O_PATH | O_DIRECTORY | O_CLOEXEC);
    if (procfd < 0) return fail("procfs", errno);
    if (fstatfs(procfd, &fs) || fs.f_type != PROC_SUPER_MAGIC) { close(procfd); return fail("procfs", EPERM); }
    close(procfd);
    procroot = open("/proc", O_PATH | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (procroot < 0) return fail("ancestor-proc", errno);
    if (native_ancestor_proc_private(procroot)) {
        int e = errno; close(procroot); return fail("ancestor-proc", e);
    }
    close(procroot);

    n = readlink("/proc/self/fd/3", link, sizeof link - 1);
    if (n < 0) return fail("ready-fd", errno);
    link[n] = 0;
    if (strncmp(link, "pipe:[", 6) || fstat(READY_FD, &st) || !S_ISFIFO(st.st_mode) ||
        (fl = fcntl(READY_FD, F_GETFL)) < 0 || (fl & O_ACCMODE) != O_WRONLY)
        return fail("ready-fd", EBADF);

    n = readlink("/proc/self/fd/4", link, sizeof link - 1);
    if (n < 0) return fail("parent-fd", errno);
    link[n] = 0;
    if (strcmp(link, "anon_inode:[pidfd]")) return fail("parent-fd", EBADF);
    if (parent_pidfd_ok()) return fail("parent-fd", errno);
    /* Readable means the supervisor has already exited. */
    if (poll(&parent, 1, 0) != 0) return fail("parent-fd", EPIPE);

    if (fcntl(READY_FD, F_SETFD, FD_CLOEXEC) || fcntl(PARENT_FD, F_SETFD, FD_CLOEXEC))
        return fail("fd-cloexec", errno);
    return 0;
}

/* Signals every namespace descendant only once the namespace is verified;
 * kill(-1) from PID 1 reaches only this PID namespace. */
static void sweep(int sig) {
    if (namespace_ready) {
        (void)kill(-1, sig);
        return;
    }
    for (int i = 0; i < MAX_CHILDREN; ++i)
        if (kids[i].pid && !kids[i].exited) (void)kill(kids[i].pid, sig);
}

static void settle(int64_t until) {
    for (;;) {
        struct pollfd p = { .fd = sigfd, .events = POLLIN };
        int64_t now;
        reap();
        if (!children_remain()) return;
        now = now_ms();
        if (now >= until) return;
        if (poll(&p, 1, (int)(until - now < TICK_MS ? until - now : TICK_MS)) > 0) drain_signals();
    }
}

static void teardown(void) {
    tearing_down = 1;
    sweep(SIGTERM);
    sweep(SIGCONT);
    settle(now_ms() + TERM_GRACE_MS);
    sweep(SIGKILL);
    settle(now_ms() + KILL_GRACE_MS);
    /* Anything left dies with PID 1 when this namespace init exits. */
}

int main(int argc, char **argv) {
    int code;
    (void)argv;
    deadline = now_ms() + STARTUP_MS;
    if (argc != 1) return 64;
    /* The supervisor passes only 0-4; anything else is closed, not used. */
    if (syscall(SYS_close_range, 5U, ~0U, 0) || setup_signals()) return 65;
    umask(077);
    if (clearenv()) return 65;

    if (check_entry() == 0 && start_session() == 0)
        while (pump(INT64_MAX, -1, 0) >= 0) {}

    code = running && stop_requested && !fail_stage ? 0 : 1;
    if (fail_stage) diag(fail_stage, fail_errno, fail_status);
    teardown();
    if (ready_open) close(READY_FD); /* failure: EOF without "R" */
    return code;
}
