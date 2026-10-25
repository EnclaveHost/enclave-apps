/* Measured desktop lifecycle supervisor. Never installed in legacy images.
 * The monitor must admit the complete profile and install network/UID policy
 * before starting this executable. It accepts no path, shell or tenant command.
 * The fixed nsinit owns the desktop mount setup, role drops and readiness check.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <limits.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CONTROL_DIR "/run/native-desktop"
#define CONTROL_SOCKET CONTROL_DIR "/control.sock"
#define INIT "/plat/native-desktop/nsinit"
#define MARKER "/desktop.profile"
#define MARKER_TEXT "KVM-BOX-MASKED/1\n"
#define GATEWAY_UID 1002
#define CLIENTS 8
#define STACK_BYTES (128 * 1024)
/* Full model verification can exceed a minute on cold/slower storage. Keep a
 * fixed outer bound and leave nsinit five seconds for failure teardown. */
#define START_MS 120000
#define STOP_MS 5000
#define CLIENT_MS 5000

enum state { STOPPED, STARTING, RUNNING, STOPPING, FAILED };
static const char *const state_names[] = { "stopped", "starting", "running", "stopping", "failed" };
static volatile sig_atomic_t terminate_requested;
static enum state status = STOPPED;
static pid_t child = -1;
static int child_fd = -1, ready_fd = -1, self_fd = -1;
static int listener = -1, init_fd = -1;
static int vcpus, memory_mib, ready_seen;
static int64_t lifecycle_deadline;
static dev_t socket_dev;
static ino_t socket_ino;
static void *child_stack;

struct client {
    int fd;
    int64_t deadline;
    unsigned char command;
    unsigned char input[2];
    size_t received, sent, length;
    unsigned char output[256];
};
static struct client clients[CLIENTS];

static int64_t now_ms(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) _exit(111);
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
static void request_termination(int sig) { (void)sig; terminate_requested = 1; }
static void close_fd(int *fd) { if (*fd >= 0) close(*fd); *fd = -1; }
static int pid_signal(int fd, int signal) {
    return syscall(SYS_pidfd_send_signal, fd, signal, NULL, 0);
}
static int bounded_integer(const char *s, int low, int high) {
    if (!s || !*s) return -1;
    unsigned long value = 0;
    for (const unsigned char *p = (const unsigned char *)s; *p; ++p) {
        if (*p < '0' || *p > '9' || value > (unsigned long)high / 10) return -1;
        value = value * 10 + *p - '0';
        if (value > (unsigned long)high) return -1;
    }
    return value >= (unsigned long)low ? (int)value : -1;
}
static int root_readonly_regular(int fd) {
    struct stat st;
    return fstat(fd, &st) == 0 && S_ISREG(st.st_mode) && st.st_uid == 0 && !(st.st_mode & 0222);
}
static int check_profile(void) {
    struct utsname name;
    if (geteuid() || getuid() || uname(&name)) return -1;
#ifndef NATIVE_SUPERVISOR_TEST
    if (strcmp(name.release, "6.6.87.2-enclave-kvm-box")) return -1;
#endif
    int fd = open(MARKER, O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0 || !root_readonly_regular(fd)) { if (fd >= 0) close(fd); return -1; }
    char marker[sizeof MARKER_TEXT + 1];
    ssize_t n = read(fd, marker, sizeof marker);
    close(fd);
    if (n != sizeof MARKER_TEXT - 1 || memcmp(marker, MARKER_TEXT, sizeof MARKER_TEXT - 1)) return -1;
    int opened = open(INIT, O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
    if (opened < 0) return -1;
    /* Standard fds may initially be closed. Keep the verified executable
     * outside 0..5 before normalizing stdio and assigning child descriptors. */
    init_fd = fcntl(opened, F_DUPFD_CLOEXEC, 6);
    close(opened);
    if (init_fd < 0 || !root_readonly_regular(init_fd)) return -1;
    struct stat st;
    if (fstat(init_fd, &st) || !(st.st_mode & 0111)) return -1;
    return 0;
}

/* A private copy of this struct is present in the clone child (no CLONE_VM).
 * All unrelated descriptors, including accepted control clients, are closed.
 * fd 3 is nsinit's readiness pipe; fd 4 is a pidfd for THIS supervisor. nsinit
 * must poll it until exit, including the parent-death-before-prctl race.
 */
struct child_args { int ready, parent, executable; };
static int child_main(void *opaque) {
    struct child_args a = *(struct child_args *)opaque;
    if (prctl(PR_SET_PDEATHSIG, SIGKILL)) _exit(126);
    struct pollfd parent = { .fd = a.parent, .events = POLLIN };
    if (poll(&parent, 1, 0) != 0) _exit(126);
    /* A new mount namespace initially inherits propagation relationships. */
    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) _exit(126);
    int r = fcntl(a.ready, F_DUPFD_CLOEXEC, 10);
    int p = fcntl(a.parent, F_DUPFD_CLOEXEC, 10);
    int e = fcntl(a.executable, F_DUPFD_CLOEXEC, 10);
    if (r < 0 || p < 0 || e < 0) _exit(126);
    if (dup2(r, 3) != 3 || dup2(p, 4) != 4 || dup3(e, 5, O_CLOEXEC) != 5) _exit(126);
    if (syscall(SYS_close_range, 6U, ~0U, 0)) _exit(126);
    sigset_t empty;
    sigemptyset(&empty);
    if (sigprocmask(SIG_SETMASK, &empty, NULL)) _exit(126);
    signal(SIGTERM, SIG_DFL); signal(SIGINT, SIG_DFL); signal(SIGCHLD, SIG_DFL); signal(SIGPIPE, SIG_DFL);
    char *const argv[] = { (char *)INIT, NULL };
    char *const env[] = { "PATH=/usr/bin:/bin", NULL };
    fexecve(5, argv, env);
    _exit(127);
}

static void begin(void) {
    if (status == STARTING || status == RUNNING || status == STOPPING || child > 0) return;
    int ready[2];
    if (pipe2(ready, O_CLOEXEC | O_NONBLOCK)) { status = FAILED; return; }
    struct child_args args = { ready[1], self_fd, init_fd };
    child = clone(child_main, (char *)child_stack + STACK_BYTES,
                  CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWIPC | CLONE_NEWUTS | SIGCHLD, &args);
    close(ready[1]);
    if (child < 0) { close(ready[0]); status = FAILED; return; }
    child_fd = syscall(SYS_pidfd_open, child, 0);
    if (child_fd < 0) {
        /* Not reaped yet, so this owned child PID cannot have been recycled. */
        kill(child, SIGKILL);
        while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {}
        child = -1; close(ready[0]); status = FAILED; return;
    }
    ready_fd = ready[0];
    ready_seen = 0;
    status = STARTING;
    lifecycle_deadline = now_ms() + START_MS;
}
static void halt(void) {
    if (child <= 0) { status = STOPPED; return; }
    if (status == STOPPING) return;
    status = STOPPING;
    lifecycle_deadline = now_ms() + STOP_MS;
    (void)pid_signal(child_fd, SIGTERM);
    close_fd(&ready_fd);
}
static void lifecycle(void) {
    if (child <= 0) return;
    int result;
    pid_t got = waitpid(child, &result, WNOHANG);
    if (got == child) {
        status = status == STOPPING ? STOPPED : FAILED;
        child = -1; close_fd(&child_fd); close_fd(&ready_fd);
        return;
    }
    if (got < 0 && errno != EINTR) { terminate_requested = 1; return; }
    if (status == STARTING && ready_fd >= 0) {
        unsigned char bytes[2];
        ssize_t n = read(ready_fd, bytes, sizeof bytes);
        if (n == 1 && bytes[0] == 'R' && !ready_seen) {
            ready_seen = 1;
        } else if (n == 0 && ready_seen) {
            status = RUNNING; close_fd(&ready_fd);
        } else if (n == 0 || n > 0 || (errno != EAGAIN && errno != EINTR)) {
            status = FAILED; close_fd(&ready_fd); (void)pid_signal(child_fd, SIGKILL);
        }
    }
    if ((status == STARTING || status == STOPPING) && now_ms() >= lifecycle_deadline) {
        if (status == STARTING) status = FAILED;
        close_fd(&ready_fd); (void)pid_signal(child_fd, SIGKILL);
    }
}
static void close_client(struct client *c) { close_fd(&c->fd); }
static void reply(struct client *c) {
    if (c->command == 'B') begin();
    else if (c->command == 'H') halt();
    int n = snprintf((char *)c->output + 4, sizeof c->output - 4,
        "{\"status\":\"%s\",\"pid\":%d,\"vcpus\":%d,\"memoryMiB\":%d}",
        state_names[status], child > 0 ? child : 0, vcpus, memory_mib);
    if (n < 1 || (size_t)n >= sizeof c->output - 4) { close_client(c); return; }
    c->output[0] = (unsigned)n >> 24; c->output[1] = (unsigned)n >> 16;
    c->output[2] = (unsigned)n >> 8; c->output[3] = (unsigned)n;
    c->length = (size_t)n + 4;
}
static void pump_client(struct client *c) {
    if (c->fd < 0) return;
    if (now_ms() >= c->deadline) { close_client(c); return; }
    if (!c->length) {
        ssize_t n = read(c->fd, c->input + c->received, sizeof c->input - c->received);
        if (n > 0) {
            c->received += (size_t)n;
            if (c->received != 1 || (c->input[0] != 'Q' && c->input[0] != 'B' && c->input[0] != 'H')) close_client(c);
        } else if (n == 0) {
            if (c->received != 1) { close_client(c); return; }
            c->command = c->input[0]; reply(c);
        } else if (errno != EAGAIN && errno != EINTR) close_client(c);
    }
    if (c->fd >= 0 && c->length) {
        ssize_t n = send(c->fd, c->output + c->sent, c->length - c->sent, MSG_NOSIGNAL);
        if (n > 0) c->sent += (size_t)n;
        else if (n == 0 || (errno != EAGAIN && errno != EINTR)) { close_client(c); return; }
        if (c->sent == c->length) close_client(c);
    }
}
static void accept_clients(void) {
    for (int attempt = 0; attempt < CLIENTS; ++attempt) {
        int fd = accept4(listener, NULL, NULL, SOCK_CLOEXEC | SOCK_NONBLOCK);
        if (fd < 0) return;
        struct ucred cred;
        socklen_t len = sizeof cred;
        if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &cred, &len) || len != sizeof cred || cred.uid != GATEWAY_UID) { close(fd); continue; }
        int slot;
        for (slot = 0; slot < CLIENTS && clients[slot].fd >= 0; ++slot) {}
        if (slot == CLIENTS) { close(fd); continue; }
        clients[slot] = (struct client){ .fd = fd, .deadline = now_ms() + CLIENT_MS };
    }
}
static int setup_socket(void) {
    struct stat st;
    if (lstat(CONTROL_DIR, &st) || !S_ISDIR(st.st_mode) || st.st_uid != 0 || st.st_gid != GATEWAY_UID || (st.st_mode & 07777) != 0750) return -1;
    if (lstat(CONTROL_SOCKET, &st) == 0 || errno != ENOENT) return -1;
    listener = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    if (listener < 0) return -1;
    struct sockaddr_un sa = { .sun_family = AF_UNIX };
    memcpy(sa.sun_path, CONTROL_SOCKET, sizeof CONTROL_SOCKET);
    mode_t old = umask(0077);
    int result = bind(listener, (struct sockaddr *)&sa, sizeof sa);
    umask(old);
    if (result) return -1;
    if (lstat(CONTROL_SOCKET, &st) || !S_ISSOCK(st.st_mode)) return -1;
    socket_dev = st.st_dev; socket_ino = st.st_ino;
    if (chown(CONTROL_SOCKET, 0, GATEWAY_UID) || chmod(CONTROL_SOCKET, 0660) || listen(listener, CLIENTS)) return -1;
    return 0;
}
static void cleanup(void) {
    if (child > 0) {
        (void)pid_signal(child_fd, SIGKILL);
        while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {}
    }
    close_fd(&listener); close_fd(&child_fd); close_fd(&ready_fd); close_fd(&self_fd); close_fd(&init_fd);
    for (int i = 0; i < CLIENTS; ++i) close_client(&clients[i]);
    struct stat st;
    if (socket_ino && lstat(CONTROL_SOCKET, &st) == 0 && st.st_dev == socket_dev && st.st_ino == socket_ino && S_ISSOCK(st.st_mode)) (void)unlink(CONTROL_SOCKET);
    if (child_stack && child_stack != MAP_FAILED) munmap(child_stack, STACK_BYTES);
}
int main(int argc, char **argv) {
    sigset_t empty;
    sigemptyset(&empty);
    if (sigprocmask(SIG_SETMASK, &empty, NULL) || signal(SIGCHLD, SIG_DFL) == SIG_ERR) return 65;
    for (int i = 0; i < CLIENTS; ++i) clients[i].fd = -1;
    if (argc != 3 || (vcpus = bounded_integer(argv[1], 1, 16)) < 0 ||
        (memory_mib = bounded_integer(argv[2], 8192, 65536)) < 0 || check_profile()) return 64;
    /* Never let desktop output inherit the hypervisor-visible console. */
    int nullfd = open("/dev/null", O_RDWR | O_NOFOLLOW | O_CLOEXEC);
    struct stat st;
    if (nullfd < 0 || fstat(nullfd, &st) || !S_ISCHR(st.st_mode) || st.st_rdev != makedev(1, 3)) return 65;
    for (int i = 0; i < 3; ++i)
        if (dup2(nullfd, i) < 0 || fcntl(i, F_SETFD, 0)) return 65;
    if (nullfd > 2) close(nullfd);
    if (clearenv()) return 65;
    signal(SIGPIPE, SIG_IGN);
    struct sigaction action = { .sa_handler = request_termination };
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGTERM, &action, NULL) || sigaction(SIGINT, &action, NULL)) return 65;
    self_fd = syscall(SYS_pidfd_open, getpid(), 0);
    child_stack = mmap(NULL, STACK_BYTES, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
    if (self_fd < 0 || child_stack == MAP_FAILED || setup_socket()) { cleanup(); return 66; }
    while (!terminate_requested) {
        lifecycle(); accept_clients();
        struct pollfd polls[CLIENTS + 2];
        polls[0] = (struct pollfd){ .fd = listener, .events = POLLIN };
        polls[1] = (struct pollfd){ .fd = ready_fd, .events = POLLIN };
        for (int i = 0; i < CLIENTS; ++i) {
            pump_client(&clients[i]);
            polls[i + 2] = (struct pollfd){ .fd = clients[i].fd, .events = clients[i].length ? POLLOUT : POLLIN };
        }
        if (poll(polls, CLIENTS + 2, 20) < 0 && errno != EINTR) break;
    }
    cleanup();
    return 0;
}
