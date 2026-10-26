/* app-seccomp.h - the seccomp filter on the APP RUNTIME, and only on it (never the front): installed by the runtime's
 * own child, after its privilege drop and PR_SET_NO_NEW_PRIVS, right before it execs the runtime. Shared by the SNP
 * guest's init (m2/dominit.c) and the NucBox domain's PID 1 (m3/domexec.c). enclave-87's ruling on the residual
 * enclave-b4 recorded after the privilege drop: an escaped runtime could still open AF_VSOCK sockets and dial the
 * host's vsock services, which a uid alone does not stop.
 *
 * x86_64 only: the guests are x86_64, and the numbers below are its syscall table's (the test checks each one against
 * <sys/syscall.h>). No kernel header is included: the image's init links musl, which ships none, so the few ABI
 * constants are spelled out here.
 *
 * What is refused, and why each action (enclave-87: "pick errno vs kill per syscall with care"):
 *   KILL the process -
 *     - any syscall not through the x86_64 entry (arch != AUDIT_ARCH_X86_64: i386's `int 0x80`, whose numbers differ,
 *       so every rule below would be bypassed) and any x32 syscall (nr with bit 30): no runtime makes one, and letting
 *       one through would bypass the whole list;
 *     - kexec_load, kexec_file_load, init_module, finit_module, delete_module: they need capabilities the dropped
 *       runtime does not have, and no user-space runtime has a reason to try; an attempt is an escape at work, so the
 *       domain ends at once.
 *   EPERM (a clean error, so a runtime or library that PROBES a feature degrades instead of dying) -
 *     - socket / socketpair with AF_VSOCK: the residual itself. The front (unfiltered) owns the domain's vsock;
 *     - io_uring_setup / io_uring_enter / io_uring_register: async runtimes probe io_uring and fall back to epoll
 *       (also off kernel-wide: kernel.io_uring_disabled=2);
 *     - setns, unshare with any CLONE_NEW* flag, clone with any CLONE_NEW* flag: no namespace for the app (user
 *       namespaces are also off kernel-wide: user.max_user_namespaces=0); plain thread and process clones pass;
 *     - ptrace, process_vm_readv, process_vm_writev, pidfd_getfd, process_madvise, kcmp: no reaching into another
 *       process - its memory, its descriptors (pidfd_getfd: the "lift the front's descriptors" vector; on the NucBox the
 *       runtime and the front share a uid, so this is a third layer after Yama and the front's non-dumpable flag;
 *       enclave-5d), its pages or its kernel objects (Yama and the uid already refuse; a crash handler that tries gets an
 *       error);
 *     - bpf, perf_event_open: probed by profilers and tracing libraries; unprivileged BPF is already off;
 *     - keyctl, add_key, request_key: the kernel keyrings, probed by some crypto and credential libraries;
 *     - userfaultfd: an old wasmtime pooling option could use it; it is an exploitation primitive.
 *   ENOSYS -
 *     - clone3: its flags are in memory, where seccomp cannot look, so a CLONE_NEW* could hide there. ENOSYS makes
 *       libc (glibc 2.34+, musl never uses clone3) fall back to clone, whose flags the rule above checks - the same
 *       choice as Docker's default profile.
 *   Everything else is allowed: the JIT (mmap, mprotect, memfd_create), epoll, threads, files, and IP and unix
 *   sockets (a run-mode app serves HTTP on loopback; egress goes to the front's forwarders).
 *
 * -> app_seccomp_install(): 0 when the filter is installed (and PR_GET_SECCOMP says mode 2), -1 with errno otherwise.
 * The caller refuses to exec the runtime on -1. */
#ifndef ENCLAVE_APP_SECCOMP_H
#define ENCLAVE_APP_SECCOMP_H
#include <stdio.h>
#include "sha256-min.h"
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/prctl.h>

/* the kernel's BPF and seccomp ABI (linux/filter.h, linux/seccomp.h, linux/audit.h) */
struct app_sock_filter { uint16_t code; uint8_t jt, jf; uint32_t k; };
struct app_sock_fprog { unsigned short len; struct app_sock_filter *filter; };
#define APP_BPF_LD_W_ABS 0x20              /* BPF_LD | BPF_W | BPF_ABS */
#define APP_BPF_JEQ_K 0x15                 /* BPF_JMP | BPF_JEQ | BPF_K */
#define APP_BPF_JGE_K 0x35                 /* BPF_JMP | BPF_JGE | BPF_K */
#define APP_BPF_JSET_K 0x45                /* BPF_JMP | BPF_JSET | BPF_K */
#define APP_BPF_RET_K 0x06                 /* BPF_RET | BPF_K */
#define APP_SECCOMP_MODE_FILTER 2
#define APP_RET_KILL_PROCESS 0x80000000U
#define APP_RET_ERRNO 0x00050000U
#define APP_RET_ALLOW 0x7fff0000U
#define APP_EPERM (APP_RET_ERRNO | (EPERM & 0xffff))
#define APP_ENOSYS (APP_RET_ERRNO | (ENOSYS & 0xffff))
#define APP_AUDIT_ARCH_X86_64 0xC000003EU
#define APP_X32_BIT 0x40000000U
#define APP_OFF_NR 0                       /* struct seccomp_data: nr, arch, instruction_pointer, args[6] */
#define APP_OFF_ARCH 4
#define APP_OFF_ARG0_LO 16                 /* little-endian: args[0]'s low 32 bits */
#define APP_AF_VSOCK 40
#define APP_CLONE_NEW_CLONE 0x7E020000U    /* NEWNS|NEWCGROUP|NEWUTS|NEWIPC|NEWUSER|NEWPID|NEWNET (clone's low byte is the exit signal) */
#define APP_CLONE_NEW_UNSHARE 0x7E020080U  /* ...and NEWTIME, for unshare */

/* x86_64 syscall numbers */
#define APP_NR_socket 41
#define APP_NR_socketpair 53
#define APP_NR_clone 56
#define APP_NR_ptrace 101
#define APP_NR_init_module 175
#define APP_NR_delete_module 176
#define APP_NR_kexec_load 246
#define APP_NR_add_key 248
#define APP_NR_request_key 249
#define APP_NR_keyctl 250
#define APP_NR_unshare 272
#define APP_NR_perf_event_open 298
#define APP_NR_setns 308
#define APP_NR_kcmp 312
#define APP_NR_process_vm_readv 310
#define APP_NR_process_vm_writev 311
#define APP_NR_finit_module 313
#define APP_NR_kexec_file_load 320
#define APP_NR_bpf 321
#define APP_NR_userfaultfd 323
#define APP_NR_io_uring_setup 425
#define APP_NR_io_uring_enter 426
#define APP_NR_io_uring_register 427
#define APP_NR_clone3 435
#define APP_NR_pidfd_getfd 438
#define APP_NR_process_madvise 440

#define APP_STMT(code, k) {(code), 0, 0, (k)}
#define APP_JUMP(code, k, jt, jf) {(code), (jt), (jf), (k)}
/* nr == N -> return ACTION (A still holds nr afterwards for the next rule) */
#define APP_RULE(nr, action) APP_JUMP(APP_BPF_JEQ_K, (nr), 0, 1), APP_STMT(APP_BPF_RET_K, (action))
/* nr == N and (the low word of arg 0 TEST K) -> return ACTION, else ALLOW; five instructions, skipped as a block */
#define APP_ARG0_EQ(nr, val, action) \
    APP_JUMP(APP_BPF_JEQ_K, (nr), 0, 4), APP_STMT(APP_BPF_LD_W_ABS, APP_OFF_ARG0_LO), \
    APP_JUMP(APP_BPF_JEQ_K, (val), 0, 1), APP_STMT(APP_BPF_RET_K, (action)), APP_STMT(APP_BPF_RET_K, APP_RET_ALLOW)
#define APP_ARG0_ANY(nr, mask, action) \
    APP_JUMP(APP_BPF_JEQ_K, (nr), 0, 4), APP_STMT(APP_BPF_LD_W_ABS, APP_OFF_ARG0_LO), \
    APP_JUMP(APP_BPF_JSET_K, (mask), 0, 1), APP_STMT(APP_BPF_RET_K, (action)), APP_STMT(APP_BPF_RET_K, APP_RET_ALLOW)

static struct app_sock_filter app_seccomp_prog[] = {
    APP_STMT(APP_BPF_LD_W_ABS, APP_OFF_ARCH),
    APP_JUMP(APP_BPF_JEQ_K, APP_AUDIT_ARCH_X86_64, 1, 0),
    APP_STMT(APP_BPF_RET_K, APP_RET_KILL_PROCESS),
    APP_STMT(APP_BPF_LD_W_ABS, APP_OFF_NR),
    APP_JUMP(APP_BPF_JGE_K, APP_X32_BIT, 0, 1),
    APP_STMT(APP_BPF_RET_K, APP_RET_KILL_PROCESS),
    APP_ARG0_EQ(APP_NR_socket, APP_AF_VSOCK, APP_EPERM),
    APP_ARG0_EQ(APP_NR_socketpair, APP_AF_VSOCK, APP_EPERM),
    APP_ARG0_ANY(APP_NR_clone, APP_CLONE_NEW_CLONE, APP_EPERM),
    APP_ARG0_ANY(APP_NR_unshare, APP_CLONE_NEW_UNSHARE, APP_EPERM),
    APP_RULE(APP_NR_clone3, APP_ENOSYS),
    APP_RULE(APP_NR_setns, APP_EPERM),
    APP_RULE(APP_NR_io_uring_setup, APP_EPERM),
    APP_RULE(APP_NR_io_uring_enter, APP_EPERM),
    APP_RULE(APP_NR_io_uring_register, APP_EPERM),
    APP_RULE(APP_NR_ptrace, APP_EPERM),
    APP_RULE(APP_NR_process_vm_readv, APP_EPERM),
    APP_RULE(APP_NR_process_vm_writev, APP_EPERM),
    APP_RULE(APP_NR_pidfd_getfd, APP_EPERM),
    APP_RULE(APP_NR_process_madvise, APP_EPERM),
    APP_RULE(APP_NR_kcmp, APP_EPERM),
    APP_RULE(APP_NR_bpf, APP_EPERM),
    APP_RULE(APP_NR_perf_event_open, APP_EPERM),
    APP_RULE(APP_NR_keyctl, APP_EPERM),
    APP_RULE(APP_NR_add_key, APP_EPERM),
    APP_RULE(APP_NR_request_key, APP_EPERM),
    APP_RULE(APP_NR_userfaultfd, APP_EPERM),
    APP_RULE(APP_NR_kexec_load, APP_RET_KILL_PROCESS),
    APP_RULE(APP_NR_kexec_file_load, APP_RET_KILL_PROCESS),
    APP_RULE(APP_NR_init_module, APP_RET_KILL_PROCESS),
    APP_RULE(APP_NR_finit_module, APP_RET_KILL_PROCESS),
    APP_RULE(APP_NR_delete_module, APP_RET_KILL_PROCESS),
    APP_STMT(APP_BPF_RET_K, APP_RET_ALLOW),
};

/* THE STATEMENT an init makes once this filter is installed on the runtime (enclave-87: positive evidence, not only the
 * absence of an error): the sha256 of the exact BPF program handed to the kernel - this sock_filter array, 8 bytes an
 * instruction on x86-64, the same bytes PTRACE_SECCOMP_GET_FILTER returns - and its length. The attester carries the hash
 * into the attested self-test (seccomp=<hash>); anyone can recompute it from this header. -> the line's length, or -1. */
#define APP_SECCOMP_RULES ((unsigned)(sizeof app_seccomp_prog / sizeof app_seccomp_prog[0]))
static int app_seccomp_statement(char *buf, size_t n) {
    char hex[65];
    sha256_min_hex(app_seccomp_prog, sizeof app_seccomp_prog, hex);
    int k = snprintf(buf, n, "seccomp sha256=%s rules=%u\n", hex, APP_SECCOMP_RULES);
    return k > 0 && (size_t)k < n ? k : -1;
}

/* needs PR_SET_NO_NEW_PRIVS already set (the caller's drop sets it); a filter cannot be removed once installed */
static int app_seccomp_install(void) {
    struct app_sock_fprog prog = {(unsigned short)(sizeof app_seccomp_prog / sizeof app_seccomp_prog[0]), app_seccomp_prog};
    if (prctl(PR_SET_SECCOMP, APP_SECCOMP_MODE_FILTER, &prog, 0, 0) != 0) return -1;
    if (prctl(PR_GET_SECCOMP, 0, 0, 0, 0) != APP_SECCOMP_MODE_FILTER) { errno = EPERM; return -1; }
    return 0;
}
#endif
