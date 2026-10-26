/* Native desktop refinement of the pinned platform filter. Reject explicit
 * writable+executable memory requests with EPERM before the kernel changes a
 * mapping. Keep the attestation scan: seccomp cannot inspect ELF program headers
 * or prove absence of writable/executable aliases of the same backing memory.
 * Installed before exec in each fresh, single-threaded role child; inherited
 * by its descendants. The unchanged platform denials take precedence.
 */
#ifndef KVMBOX_NATIVE_WX_POLICY_H
#define KVMBOX_NATIVE_WX_POLICY_H
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>
#include "platform-policy/app-seccomp.h"
#if !defined(__x86_64__) || defined(__ILP32__)
#error Native desktop memory policy requires the x86-64 syscall ABI
#endif
#define NWX_OFF_ARG2_LO 32
#define NWX_BPF_AND_K 0x54
#define NWX_PROT_WX 6U
#define NWX_READ_IMPLIES_EXEC 0x00400000U
#define NWX_SHM_RDONLY 010000U
#define NWX_SHM_EXEC 0100000U
/* All three protection-changing calls have prot in argument 2. Other
 * protection bits must not hide the simultaneous WRITE and EXEC bits. */
#define NWX_PROTECTION(nr) \
    APP_JUMP(APP_BPF_JEQ_K, (nr), 0, 5), \
    APP_STMT(APP_BPF_LD_W_ABS, NWX_OFF_ARG2_LO), \
    APP_STMT(NWX_BPF_AND_K, NWX_PROT_WX), \
    APP_JUMP(APP_BPF_JEQ_K, NWX_PROT_WX, 0, 1), \
    APP_STMT(APP_BPF_RET_K, APP_EPERM), \
    APP_STMT(APP_BPF_RET_K, APP_RET_ALLOW)
static const struct app_sock_filter native_wx_rules[] = {
    NWX_PROTECTION(SYS_mmap),
    NWX_PROTECTION(SYS_mprotect),
    NWX_PROTECTION(SYS_pkey_mprotect),
    /* personality(-1) queries remain valid. Refuse setting READ_IMPLIES_EXEC,
     * which could otherwise add EXEC to a requested READ|WRITE mapping. */
    APP_JUMP(APP_BPF_JEQ_K, SYS_personality, 0, 5),
    APP_STMT(APP_BPF_LD_W_ABS, APP_OFF_ARG0_LO),
    APP_JUMP(APP_BPF_JEQ_K, 0xffffffffU, 2, 0),
    APP_JUMP(APP_BPF_JSET_K, NWX_READ_IMPLIES_EXEC, 0, 1),
    APP_STMT(APP_BPF_RET_K, APP_EPERM),
    APP_STMT(APP_BPF_RET_K, APP_RET_ALLOW),
    /* shmat defaults to writable; SHM_EXEC is safe only with SHM_RDONLY. */
    APP_JUMP(APP_BPF_JEQ_K, SYS_shmat, 0, 5),
    APP_STMT(APP_BPF_LD_W_ABS, NWX_OFF_ARG2_LO),
    APP_STMT(NWX_BPF_AND_K, NWX_SHM_RDONLY | NWX_SHM_EXEC),
    APP_JUMP(APP_BPF_JEQ_K, NWX_SHM_EXEC, 0, 1),
    APP_STMT(APP_BPF_RET_K, APP_EPERM),
    APP_STMT(APP_BPF_RET_K, APP_RET_ALLOW),
    APP_STMT(APP_BPF_RET_K, APP_RET_ALLOW),
};
#define NATIVE_SECCOMP_RULES (APP_SECCOMP_RULES - 1 + sizeof native_wx_rules / sizeof native_wx_rules[0])
static struct app_sock_filter native_seccomp_prog[NATIVE_SECCOMP_RULES];
static inline int native_seccomp_prepare(void) {
    /* The vendored program remains byte-for-byte pinned. Replace only its final
     * fall-through ALLOW, preserving its architecture checks and early denials. */
    const struct app_sock_filter *last = &app_seccomp_prog[APP_SECCOMP_RULES - 1];
    (void)app_seccomp_install; (void)app_seccomp_statement;
    if (last->code != APP_BPF_RET_K || last->k != APP_RET_ALLOW || last->jt || last->jf) {
        errno = EINVAL; return -1;
    }
    memcpy(native_seccomp_prog, app_seccomp_prog, sizeof app_seccomp_prog - sizeof *last);
    memcpy(native_seccomp_prog + APP_SECCOMP_RULES - 1, native_wx_rules, sizeof native_wx_rules);
    return 0;
}
static inline int native_seccomp_statement(char *out, size_t length) {
    char hex[65];
    if (native_seccomp_prepare()) return -1;
    sha256_min_hex(native_seccomp_prog, sizeof native_seccomp_prog, hex);
    int n = snprintf(out, length, "seccomp sha256=%s rules=%u\n", hex, (unsigned)NATIVE_SECCOMP_RULES);
    return n > 0 && (size_t)n < length ? n : -1;
}
static inline int native_seccomp_install(void) {
    long personality = syscall(SYS_personality, 0xffffffffUL);
    if (personality < 0) return -1;
    if (personality & NWX_READ_IMPLIES_EXEC) { errno = EPERM; return -1; }
    if (native_seccomp_prepare()) return -1;
    struct app_sock_fprog prog = {(unsigned short)NATIVE_SECCOMP_RULES, native_seccomp_prog};
    if (prctl(PR_SET_SECCOMP, APP_SECCOMP_MODE_FILTER, &prog, 0, 0)) return -1;
    if (prctl(PR_GET_SECCOMP, 0, 0, 0, 0) != APP_SECCOMP_MODE_FILTER) { errno = EPERM; return -1; }
    return 0;
}
#endif
