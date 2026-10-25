/* Fixed confinement for the measured desktop's root input udev daemon.
 * Requires a private PID/mount namespace, read-only root/sysfs, private /run,
 * input-only /dev and no inherited descriptors except null/log stdio. This
 * helper remains trusted code; it is not a sandbox for arbitrary root apps.
 */
#ifndef KVMBOX_UDEV_POLICY_H
#define KVMBOX_UDEV_POLICY_H
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <linux/audit.h>
#include <linux/capability.h>
#include <linux/filter.h>
#include <linux/netlink.h>
#include <linux/seccomp.h>
#include <sched.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error Native desktop udev confinement requires x86_64
#endif

#define UDEV_CAPS ((1ULL << CAP_CHOWN) | \
                   (1ULL << CAP_FOWNER) | (1ULL << CAP_NET_ADMIN))

static int udev_drop_capabilities(void) {
    if (getuid() || geteuid()) { errno = EPERM; return -1; }
    /* CAP_NET_ADMIN is needed for libudev's userspace multicast. Socket
     * filtering below permits only NETLINK_KOBJECT_UEVENT, never NETFILTER or
     * ROUTE, and denies all ioctl calls and other network socket families.
     * The caller must not supply previously opened network descriptors.
     */
    int end = 0;
    for (int cap = 0; cap < 64; ++cap) {
        if (prctl(PR_CAPBSET_READ, cap, 0, 0, 0) < 0) {
            if (errno != EINVAL) return -1;
            end = 1; break;
        }
        if (!(UDEV_CAPS & (1ULL << cap)) && prctl(PR_CAPBSET_DROP, cap, 0, 0, 0)) return -1;
    }
    if (!end) { errno = EOVERFLOW; return -1; }
    if (prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0)) return -1;
    struct __user_cap_header_struct h = { .version = _LINUX_CAPABILITY_VERSION_3, .pid = 0 };
    struct __user_cap_data_struct d[2] = {
        { .effective = (uint32_t)UDEV_CAPS, .permitted = (uint32_t)UDEV_CAPS },
        { .effective = (uint32_t)(UDEV_CAPS >> 32), .permitted = (uint32_t)(UDEV_CAPS >> 32) }
    };
    return syscall(SYS_capset, &h, d);
}

static int udev_install_seccomp(void) {
    struct sock_filter f[256];
    size_t n = 0;
#define ST(code_, value_) do { f[n++] = (struct sock_filter)BPF_STMT(code_, value_); } while (0)
#define JP(code_, value_, yes_, no_) do { f[n++] = (struct sock_filter)BPF_JUMP(code_, value_, yes_, no_); } while (0)
#define DENY (SECCOMP_RET_ERRNO | EPERM)
#define ARG(i) ((unsigned int)offsetof(struct seccomp_data, args[i]))
    ST(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch));
    JP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0);
    ST(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS);
    ST(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr));
    JP(BPF_JMP | BPF_JSET | BPF_K, 0x40000000, 0, 1);
    ST(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS);

    size_t socket_jump = n;
    JP(BPF_JMP | BPF_JEQ | BPF_K, SYS_socket, 0, 0);
    ST(BPF_LD | BPF_W | BPF_ABS, ARG(0));
    JP(BPF_JMP | BPF_JEQ | BPF_K, AF_UNIX, 0, 1);
    ST(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    JP(BPF_JMP | BPF_JEQ | BPF_K, AF_NETLINK, 1, 0);
    ST(BPF_RET | BPF_K, DENY);
    ST(BPF_LD | BPF_W | BPF_ABS, ARG(2));
    JP(BPF_JMP | BPF_JEQ | BPF_K, NETLINK_KOBJECT_UEVENT, 1, 0);
    ST(BPF_RET | BPF_K, DENY);
    ST(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    f[socket_jump].jf = (unsigned char)(n - socket_jump - 1);

    size_t pair_jump = n;
    JP(BPF_JMP | BPF_JEQ | BPF_K, SYS_socketpair, 0, 0);
    ST(BPF_LD | BPF_W | BPF_ABS, ARG(0));
    JP(BPF_JMP | BPF_JEQ | BPF_K, AF_UNIX, 1, 0);
    ST(BPF_RET | BPF_K, DENY);
    ST(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    f[pair_jump].jf = (unsigned char)(n - pair_jump - 1);

    size_t clone_jump = n;
    JP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone, 0, 0);
    ST(BPF_LD | BPF_W | BPF_ABS, ARG(0));
    JP(BPF_JMP | BPF_JSET | BPF_K, CLONE_NEWCGROUP | CLONE_NEWIPC | CLONE_NEWNET |
       CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUSER | CLONE_NEWUTS | CLONE_NEWTIME, 0, 1);
    ST(BPF_RET | BPF_K, DENY);
    ST(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    f[clone_jump].jf = (unsigned char)(n - clone_jump - 1);

    /* libc falls back to clone for threads/fork when clone3 is unavailable. */
    JP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone3, 0, 1);
    ST(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOSYS);
    static const int denied[] = {
        SYS_ioctl, SYS_mknod, SYS_mknodat, SYS_unshare, SYS_setns, SYS_mount, SYS_umount2, SYS_pivot_root,
        SYS_chroot, SYS_fsopen, SYS_fsconfig, SYS_fsmount, SYS_fspick, SYS_move_mount,
        SYS_open_tree, SYS_mount_setattr, SYS_bpf, SYS_perf_event_open,
        SYS_io_uring_setup, SYS_io_uring_enter, SYS_io_uring_register,
        SYS_ptrace, SYS_process_vm_readv, SYS_process_vm_writev, SYS_pidfd_getfd,
        SYS_open_by_handle_at, SYS_init_module, SYS_finit_module, SYS_delete_module,
        SYS_kexec_load, SYS_kexec_file_load, SYS_reboot, SYS_swapon, SYS_swapoff,
        SYS_add_key, SYS_request_key, SYS_keyctl, SYS_userfaultfd,
        SYS_iopl, SYS_ioperm
    };
    for (size_t i = 0; i < sizeof denied / sizeof denied[0]; ++i) {
        JP(BPF_JMP | BPF_JEQ | BPF_K, denied[i], 0, 1);
        ST(BPF_RET | BPF_K, DENY);
    }
    ST(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    struct sock_fprog p = { .len = (unsigned short)n, .filter = f };
    if (n > sizeof f / sizeof f[0] || prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) return -1;
    return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &p);
#undef ST
#undef JP
#undef DENY
#undef ARG
}
#endif
