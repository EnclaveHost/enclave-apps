/* Disposable KVM-only layout probe. Fixtures are NOT authenticated models or
 * a GPU worker. No host mounts, network devices, input or GPU are passed in. */
#define _GNU_SOURCE
#include "native-root.h"
#include "role-policy.h"
#include "udev-policy.h"
#include "home-seed.h"
#include "desktop-auth.h"
#include <sched.h>
#include <glob.h>
#include <signal.h>
#include <stdlib.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>

#define CHECK(x) do { if (!(x)) { dprintf(2,"ROOT_PROBE_FAIL line=%d errno=%d %s\n",__LINE__,errno,strerror(errno)); return 90; } } while (0)
static char desktop_pty[128];
static int make_dir(const char *path, mode_t mode) {
    return mkdir(path, mode) == 0 || errno == EEXIST ? 0 : -1;
}
static int put(const char *path, const char *value) {
    int f = open(path,O_WRONLY|O_CREAT|O_EXCL,0444);
    if (f < 0) return -1;
    int r = write(f,value,strlen(value)) == (ssize_t)strlen(value) ? 0 : -1;
    close(f); return r;
}
static int home_checks(void) {
    CHECK(put("/home/desktop/keep", "preserve") == 0);
    CHECK(native_seed_home() == -1 && errno == ENOTEMPTY);
    CHECK(access("/home/desktop/keep",F_OK) == 0 && unlink("/home/desktop/keep") == 0);
    CHECK(chown("/home/desktop",0,0) == 0);
    CHECK(native_seed_home() == -1 && errno == EPERM);
    CHECK(chown("/home/desktop",1000,1000) == 0);
    CHECK(mount("tmpfs","/home","tmpfs",MS_NOSUID|MS_NODEV,"mode=0755") == 0);
    CHECK(symlink("/tmp","/home/desktop") == 0);
    CHECK(native_seed_home() == -1 && (errno == ELOOP || errno == ENOTDIR));
    CHECK(umount("/home") == 0);
    CHECK(mount(NULL,"/home/desktop",NULL,MS_REMOUNT|MS_RDONLY|MS_NOSUID|MS_NODEV,NULL) == 0);
    CHECK(native_seed_home() == -1 && errno == EPERM);
    CHECK(mount(NULL,"/home/desktop",NULL,MS_REMOUNT|MS_NOSUID|MS_NODEV,NULL) == 0);
    CHECK(native_seed_home() == 0);
    CHECK(native_seed_home() == -1 && errno == ENOTEMPTY);
    CHECK(sethostname("kvm-box",7) == 0 && native_desktop_auth() == 0);
    CHECK(native_desktop_auth() == -1 && errno == EEXIST);
    struct stat st;
    CHECK(stat("/run/user/1000/Xauthority",&st) == 0 && st.st_uid == 1000 && st.st_gid == 1000 && (st.st_mode & 07777) == 0600 && st.st_size == 52);
    DIR *ds = opendir("/proc/self/fd"); CHECK(ds != NULL);
    struct dirent *entry;
    while ((entry = readdir(ds))) {
        if (entry->d_name[0] == '.') continue;
        int fd = atoi(entry->d_name); CHECK(fd <= 4 || fd == dirfd(ds));
    }
    closedir(ds);
    dprintf(1,"NATIVE_HOME_SEED_PASS files=%zu bytes=%zu noMerge=true authPrivate=true\n", (size_t)HS_EXPECT_FILES, HS_EXPECT_BYTES);
    return 0;
}
static int role_check(uid_t uid) {
    CHECK(syscall(SYS_close_range,3U,~0U,0) == 0);
    CHECK(native_enter_role(uid) == 0);
    struct sockaddr_un a = { .sun_family=AF_UNIX };
    strcpy(a.sun_path,"/run/shield-worker/gpu0");
    int s = socket(AF_UNIX,SOCK_STREAM|SOCK_CLOEXEC,0); CHECK(s >= 0);
    int r = connect(s,(struct sockaddr *)&a,sizeof a);
    if (uid == 1001) {
        struct ucred peer; socklen_t len = sizeof peer;
        CHECK(r == 0 && getsockopt(s,SOL_SOCKET,SO_PEERCRED,&peer,&len) == 0 && peer.uid == 0);
    } else CHECK(r == -1 && errno == EACCES);
    close(s);
    if (uid == 1001 || uid == 1003) CHECK(open(desktop_pty,O_WRONLY|O_NOCTTY|O_CLOEXEC) == -1 && errno == EACCES);
    CHECK(access("/dev/kvm",F_OK) == -1 && errno == ENOENT);
    CHECK(access("/dev/dri",F_OK) == -1 && errno == ENOENT);
    CHECK(access("/dev/tpm0",F_OK) == -1 && errno == ENOENT);
    CHECK(access("/dev/vsock",F_OK) == -1 && errno == ENOENT);
    CHECK(access("/dev/nvidia0",F_OK) == -1 && errno == ENOENT);
    if (uid == 1000) {
        CHECK(chmod("/dev/uhid",0666) == -1 && errno == EROFS);
        CHECK(chmod("/dev/uinput",0666) == -1 && errno == EROFS);
        int tty = posix_openpt(O_RDWR|O_NOCTTY|O_CLOEXEC); CHECK(tty >= 0);
        CHECK(grantpt(tty) == 0 && unlockpt(tty) == 0);
        int slave = open(ptsname(tty),O_RDWR|O_NOCTTY|O_CLOEXEC); CHECK(slave >= 0);
        struct stat st; CHECK(fstat(slave,&st) == 0 && st.st_uid == 1000 && (st.st_mode & 0777) == 0600);
        close(slave); close(tty);
        int u = open("/dev/uhid",O_RDWR|O_CLOEXEC); CHECK(u >= 0); close(u);
    }
    dprintf(1,"ROOT_ROLE_PASS uid=%u worker=%s\n",uid,uid==1001?"root-peer":"denied");
    return 0;
}
static int udev_check(void) {
    CHECK(syscall(SYS_close_range,3U,~0U,0) == 0);
    CHECK(udev_drop_capabilities() == 0 && udev_install_seccomp() == 0);
    CHECK(open("/dev/uhid",O_RDWR|O_CLOEXEC) == -1 && errno == EACCES);
    CHECK(chmod("/dev/uhid",0666) == -1 && errno == EROFS);
    CHECK(chmod("/dev/null",0600) == -1 && errno == EROFS);
    CHECK(open("/proc/sysrq-trigger",O_WRONLY|O_CLOEXEC) == -1 && errno == EROFS);
    CHECK(open("/proc/irq/default_smp_affinity",O_WRONLY|O_CLOEXEC) == -1 && errno == EROFS);
    glob_t pci;
    CHECK(glob("/proc/bus/pci/*/*",0,NULL,&pci) == 0 && pci.gl_pathc > 0);
    for (size_t i=0;i<pci.gl_pathc;i++) CHECK(open(pci.gl_pathv[i],O_WRONLY|O_CLOEXEC) == -1 && errno == EROFS);
    globfree(&pci);
    int f = open("/dev/null",O_WRONLY|O_CLOEXEC); CHECK(f >= 0 && write(f,"x",1) == 1); close(f);
    f = open("/dev/urandom",O_RDONLY|O_CLOEXEC); char b; CHECK(f >= 0 && read(f,&b,1) == 1); close(f);
    dprintf(1,"ROOT_UDEV_HARDENING_PASS\n"); return 0;
}
static int child_test(int which) {
    CHECK(getpid() == 1);
    if (which != 6) CHECK(mount(NULL,"/",NULL,MS_REC|MS_PRIVATE,NULL) == 0);
    if (which == 1) {
        /* A bind remount alone cannot clear the original superblock's RO bit.
         * Use a fresh writable source mount confined to this test namespace. */
        CHECK(mount("tmpfs","/run/native-desktop/models","tmpfs",0,"mode=0755") == 0);
        struct statfs fs; CHECK(statfs("/run/native-desktop/models",&fs) == 0 && !(fs.f_flags & ST_RDONLY));
    }
    if (which == 2) {
        CHECK(mount("tmpfs","/plat/native-desktop","tmpfs",0,"mode=0755") == 0);
        CHECK(symlink("/","/plat/native-desktop/rootfs") == 0);
    }
    if (which == 3) {
        CHECK(mkdir("/run/native-desktop/root",0700) == 0);
        CHECK(put("/run/native-desktop/root/keep","retained") == 0);
    }
    if (which == 4) {
        CHECK(mount("tmpfs","/run/native-desktop","tmpfs",0,"mode=0750,gid=1002") == 0);
        CHECK(mkdir("/run/native-desktop/models",0755) == 0);
        CHECK(mount("tmpfs","/run/native-desktop/models","tmpfs",MS_RDONLY,"mode=0755") == 0);
    }
    if (which == 5) CHECK(mount("tmpfs","/dev/input","tmpfs",0,"mode=0755") == 0);
    if (which == 7) CHECK(mount("proc","/proc","proc",0,NULL) == 0);
    if (which == 8) {
        struct sock_filter f[] = {
            BPF_STMT(BPF_LD|BPF_W|BPF_ABS,offsetof(struct seccomp_data,nr)),
            BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,SYS_chroot,0,1),
            BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ERRNO|EPERM),
            BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ALLOW)
        };
        struct sock_fprog p = {.len=4,.filter=f};
        CHECK(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0) == 0 && prctl(PR_SET_SECCOMP,SECCOMP_MODE_FILTER,&p) == 0);
    }
    struct stat before3,before4,after3,after4;
    CHECK(fstat(3,&before3) == 0 && fstat(4,&before4) == 0);
    int r = native_prepare_root();
    if (which) {
        CHECK(r == -1);
        const int expected[] = {0, EPERM, ELOOP, ENOTEMPTY, ENOENT, EPERM, EPERM, EPERM, EPERM};
        CHECK(errno == expected[which]);
        if (which == 3) CHECK(access("/run/native-desktop/root/keep",F_OK) == 0);
        if (which == 8) {
            struct stat node; CHECK(stat("/dev/uinput",&node) == 0 && node.st_uid == 0 && (node.st_mode & 07777) == 0600);
            CHECK(stat("/dev/uhid",&node) == 0 && node.st_uid == 0 && (node.st_mode & 07777) == 0600);
            int target = open("/run/native-desktop/root",O_PATH|O_DIRECTORY|O_CLOEXEC); CHECK(target >= 0);
            CHECK(nr_mount_root(target,0) == 0); close(target);
            char cwd[16]; CHECK(getcwd(cwd,sizeof cwd) && !strcmp(cwd,"/"));
        }
        CHECK(fstat(3,&after3) == 0 && fstat(4,&after4) == 0);
        CHECK(before3.st_ino == after3.st_ino && before4.st_ino == after4.st_ino);
        dprintf(1,"ROOT_NEGATIVE_PASS case=%d\n",which); return 0;
    }
    CHECK(r == 0);
    CHECK(fstat(3,&after3) == 0 && fstat(4,&after4) == 0);
    CHECK(before3.st_ino == after3.st_ino && before4.st_ino == after4.st_ino);
    DIR *fds = opendir("/proc/self/fd"); CHECK(fds != NULL);
    struct dirent *entry;
    while ((entry = readdir(fds))) {
        if (entry->d_name[0] == '.') continue;
        int fd = atoi(entry->d_name);
        CHECK(fd <= 4 || fd == dirfd(fds));
    }
    closedir(fds);
    const char *ro[] = {"/root-write-probe","/models/write-probe","/sys/write-probe","/proc/sys/kernel/hostname"};
    for (size_t i=0;i<sizeof ro/sizeof ro[0];i++) {
        int f = open(ro[i], O_WRONLY | (i==3?0:O_CREAT),0600);
        CHECK(f == -1 && errno == EROFS);
    }
    CHECK(access("/models/fixture",F_OK) == 0);
    CHECK(access("/run/native-desktop",F_OK) == -1 && errno == ENOENT);
    CHECK(put("/run/kvm-box-native-input-probe","INPUT-PROBE/1\n") == 0);
    CHECK(home_checks() == 0);
    pid_t uc = fork(); CHECK(uc >= 0);
    if (!uc) _exit(udev_check());
    int us; CHECK(waitpid(uc,&us,0) == uc && WIFEXITED(us) && WEXITSTATUS(us) == 0);
    /* Real desktop-owned slave; retain its master only in the trusted probe
     * parent so other role children can test denied terminal writes. */
    CHECK(setegid(1000) == 0 && seteuid(1000) == 0);
    int desktop_master = posix_openpt(O_RDWR|O_NOCTTY|O_CLOEXEC); CHECK(desktop_master >= 0);
    CHECK(grantpt(desktop_master) == 0 && unlockpt(desktop_master) == 0);
    const char *slave_name = ptsname(desktop_master); CHECK(slave_name && strlen(slave_name) < sizeof desktop_pty);
    strcpy(desktop_pty,slave_name);
    CHECK(seteuid(0) == 0 && setegid(0) == 0);
    for (uid_t uid=1000;uid<=1003;uid++) {
        pid_t c = fork(); CHECK(c >= 0);
        if (!c) _exit(role_check(uid));
        int st; CHECK(waitpid(c,&st,0) == c && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    }
    close(desktop_master);
    CHECK(write(3,"L",1) == 1);
    CHECK(syscall(SYS_close_range,3U,~0U,0) == 0);
    dprintf(1,"ROOT_LAYOUT_PASS\n");
    char *const args[] = { "/bin/busybox","sh","/probe.sh",NULL };
    char *const env[] = {"PATH=/usr/sbin:/usr/bin:/sbin:/bin","HOME=/root",NULL};
    execve(args[0],args,env); return 127;
}
static int run(void) {
    struct utsname u;
    CHECK(getpid() == 1 && getuid() == 0 && uname(&u) == 0 && !strcmp(u.release,NR_KERNEL));
    CHECK(mount("devtmpfs","/dev","devtmpfs",MS_NOSUID,"mode=0755") == 0);
    CHECK(mount("proc","/proc","proc",MS_NOSUID|MS_NODEV|MS_NOEXEC,NULL) == 0);
    CHECK(mount("sysfs","/sys","sysfs",MS_RDONLY|MS_NOSUID|MS_NODEV|MS_NOEXEC,NULL) == 0);
    CHECK(mount("tmpfs","/run","tmpfs",MS_NOSUID|MS_NODEV,"mode=0755,size=32m") == 0);
    CHECK(make_dir("/dev/input",0755) == 0);
    CHECK(make_dir("/plat",0755) == 0 && make_dir("/plat/native-desktop",0755) == 0);
    CHECK(make_dir("/plat/native-desktop/rootfs",0755) == 0);
    CHECK(make_dir("/models",0755) == 0);
    /* The historical cpio omits '.', so normalize the test-only initramfs
     * root to the immutable image-root ownership/permissions contract. */
    CHECK(chown("/",0,0) == 0 && chmod("/",0755) == 0);
    CHECK(mount("/","/plat/native-desktop/rootfs",NULL,MS_BIND,NULL) == 0);
    CHECK(mount(NULL,"/plat/native-desktop/rootfs",NULL,MS_REMOUNT|MS_BIND|MS_RDONLY|MS_NOSUID|MS_NODEV,NULL) == 0);
    CHECK(mkdir("/run/native-desktop",0750) == 0 && chown("/run/native-desktop",0,1002) == 0);
    CHECK(mkdir("/run/native-desktop/models",0755) == 0);
    CHECK(mount("tmpfs","/run/native-desktop/models","tmpfs",MS_NOSUID|MS_NODEV,"mode=0755") == 0);
    CHECK(put("/run/native-desktop/models/fixture","NOT AUTHENTICATED MODEL DATA\n") == 0);
    CHECK(mount(NULL,"/run/native-desktop/models",NULL,MS_REMOUNT|MS_RDONLY|MS_NOSUID|MS_NODEV,NULL) == 0);
    struct sockaddr_un a = {.sun_family=AF_UNIX}; strcpy(a.sun_path,"/run/native-desktop/worker.sock");
    int s = socket(AF_UNIX,SOCK_STREAM|SOCK_CLOEXEC,0); CHECK(s >= 0);
    CHECK(bind(s,(struct sockaddr *)&a,sizeof a) == 0 && listen(s,8) == 0);
    CHECK(chown(a.sun_path,0,1001) == 0 && chmod(a.sun_path,0660) == 0);
    int parent = (int)syscall(SYS_pidfd_open,getpid(),0); CHECK(parent >= 0);
    parent = fcntl(parent,F_DUPFD_CLOEXEC,20); CHECK(parent >= 0);
    for (int which=1;which<=9;which++) {
        int pipefd[2]; CHECK(pipe2(pipefd,O_CLOEXEC) == 0);
        int w = fcntl(pipefd[1],F_DUPFD_CLOEXEC,30); CHECK(w >= 0);
        pid_t c = (pid_t)syscall(SYS_clone,CLONE_NEWPID|(which==6?0:CLONE_NEWNS)|CLONE_NEWIPC|CLONE_NEWUTS|SIGCHLD,0,0,0,0);
        CHECK(c >= 0);
        if (!c) {
            CHECK(dup2(w,3) == 3 && dup2(parent,4) == 4);
            CHECK(syscall(SYS_close_range,5U,~0U,0) == 0);
            _exit(child_test(which == 9 ? 0 : which));
        }
        close(w); close(pipefd[1]);
        int st; CHECK(waitpid(c,&st,0) == c && WIFEXITED(st) && WEXITSTATUS(st) == 0);
        char marker=0; int n=read(pipefd[0],&marker,1); close(pipefd[0]);
        CHECK(which == 9 ? n == 1 && marker == 'L' : n == 0);
        if (which == 3) { CHECK(unlink("/run/native-desktop/root/keep") == 0); CHECK(rmdir("/run/native-desktop/root") == 0); }
    }
    int peer = accept4(s,NULL,NULL,SOCK_NONBLOCK|SOCK_CLOEXEC); CHECK(peer >= 0);
    struct ucred cred; socklen_t length = sizeof cred;
    CHECK(getsockopt(peer,SOL_SOCKET,SO_PEERCRED,&cred,&length) == 0 && cred.uid == 1001);
    close(peer);
    CHECK(access("/run/kvm-box-native-input-probe",F_OK) == -1 && errno == ENOENT);
    dprintf(1,"ROOT_PARENT_ISOLATION_PASS workerClientUid=1001\n");
    dprintf(1,"NATIVE_ROOT_PROBE_PASS\n"); return 0;
}
int main(void) {
    struct utsname u;
    if (getpid() != 1 || getuid() || uname(&u) || strcmp(u.release,NR_KERNEL)) return 64;
    int r = run(); dprintf(1,"NATIVE_ROOT_PROBE_EXIT=%d\n",r); sync();
    reboot(RB_POWER_OFF); return r;
}
