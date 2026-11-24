#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>
/* Disposable-guest availability probe. Opening /dev/snapshot invokes PM
 * prepare notifiers and takes global hibernation state. No freeze/restore
 * ioctls are issued. Never run this in an existing application guest. */
static void report(const char *path) {
    char data[4096]; int fd = open(path, O_RDONLY|O_CLOEXEC);
    if (fd < 0) { printf("SNAPPROBE %s ERROR %d %s\n", path, errno, strerror(errno)); return; }
    ssize_t n = read(fd, data, sizeof(data)-1); int e=errno; close(fd);
    if(n<0) {printf("SNAPPROBE %s ERROR %d\n",path,e); return;}
    data[n]=0; for(ssize_t i=0;i<n;i++) if(data[i]=='\n') data[i]=' ';
    printf("SNAPPROBE %s = %s\n",path,data);
}
int main(void) {
    if(getpid()!=1) return 64;
    mkdir("/dev",0755); mkdir("/proc",0755); mkdir("/sys",0755);
    mount("devtmpfs","/dev","devtmpfs",MS_NOSUID,NULL);
    mount("proc","/proc","proc",MS_NOSUID|MS_NODEV|MS_NOEXEC,NULL);
    mount("sysfs","/sys","sysfs",MS_NOSUID|MS_NODEV|MS_NOEXEC,NULL);
    int serial=open("/dev/console",O_WRONLY|O_NOCTTY);
    if(serial>=0){dup2(serial,1);dup2(serial,2);if(serial>2)close(serial);}
    setvbuf(stdout,NULL,_IONBF,0);
    printf("SNAPPROBE BEGIN availability-no-freeze-v2\n");
    mkdir("/sys/kernel/security",0700);
    mount("securityfs","/sys/kernel/security","securityfs",MS_NOSUID|MS_NODEV|MS_NOEXEC,NULL);
    report("/proc/cmdline");report("/sys/power/state");report("/sys/power/disk");
    report("/sys/power/resume");report("/sys/bus/vmbus/hibernation");
    report("/sys/kernel/security/lockdown");report("/proc/meminfo");
    struct stat st;
    if(lstat("/dev/snapshot",&st)<0 && errno==ENOENT) mknod("/dev/snapshot",S_IFCHR|0600,makedev(10,231));
    int fd=open("/dev/snapshot",O_RDONLY|O_CLOEXEC);
    if(fd<0) printf("SNAPPROBE snapshot_open ERROR %d %s\n",errno,strerror(errno));
    else {printf("SNAPPROBE snapshot_open OK (no ioctls)\n"); if(close(fd)<0) printf("SNAPPROBE snapshot_close ERROR %d\n",errno);else printf("SNAPPROBE snapshot_close OK\n");}
    printf("SNAPPROBE COMPLETE availability-no-freeze-v2\n");
    sleep(2);reboot(RB_POWER_OFF);for(;;)pause();
}
