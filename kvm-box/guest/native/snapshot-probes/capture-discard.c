#define _GNU_SOURCE
#include <errno.h>
#include <linux/suspend_ioctls.h>
#include <sys/ioctl.h>
#include <stdint.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>
/* Disposable synthetic guest only: freeze, capture, read and discard.
 * No restore, snapshot persistence, credentials, attached disks or network.
 * Must run with an independent host watchdog. Never run in an app guest. */
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
    printf("SNAPPROBE BEGIN capture-discard-v1\n");
    mkdir("/sys/kernel/security",0700);
    mount("securityfs","/sys/kernel/security","securityfs",MS_NOSUID|MS_NODEV|MS_NOEXEC,NULL);
    report("/proc/cmdline");report("/sys/power/state");report("/sys/power/disk");
    report("/sys/power/resume");report("/sys/bus/vmbus/hibernation");
    report("/sys/kernel/security/lockdown");report("/proc/meminfo");
    struct stat st;
    if(lstat("/dev/snapshot",&st)<0 && errno==ENOENT) mknod("/dev/snapshot",S_IFCHR|0600,makedev(10,231));
    int passed=0;
    int fd=open("/dev/snapshot",O_RDONLY|O_CLOEXEC);
    if(fd<0) { printf("SNAPPROBE snapshot_open ERROR %d %s\n",errno,strerror(errno)); goto done; }
    if(ioctl(fd,SNAPSHOT_PREF_IMAGE_SIZE,128UL*1024*1024)<0){printf("SNAPPROBE preferred_size ERROR %d\n",errno);goto close_fd;}
    printf("SNAPPROBE FREEZE begin\n");
    if(ioctl(fd,SNAPSHOT_FREEZE,0)<0){printf("SNAPPROBE FREEZE ERROR %d\n",errno);goto close_fd;}
    int original=-1;
    printf("SNAPPROBE CREATE_IMAGE begin\n");
    if(ioctl(fd,SNAPSHOT_CREATE_IMAGE,&original)<0){printf("SNAPPROBE CREATE_IMAGE ERROR %d\n",errno);goto unfreeze;}
    printf("SNAPPROBE CREATE_IMAGE OK original=%d\n",original);
    if(original!=1){printf("SNAPPROBE unexpected resume branch\n");goto free_image;}
    __kernel_loff_t size=0;
    if(ioctl(fd,SNAPSHOT_GET_IMAGE_SIZE,&size)<0){printf("SNAPPROBE IMAGE_SIZE ERROR %d\n",errno);goto free_image;}
    printf("SNAPPROBE IMAGE_SIZE %lld\n",(long long)size);
    if(size<=0 || size>512LL*1024*1024){printf("SNAPPROBE image exceeds diagnostic bound\n");goto free_image;}
    unsigned char bytes[8192];long long count=0;uint64_t digest=14695981039346656037ULL;ssize_t n;
    while((n=read(fd,bytes,sizeof(bytes)))>0){
        count+=n;if(count>size){printf("SNAPPROBE image exceeds declared size\n");goto free_image;}
        for(ssize_t j=0;j<n;j++)digest=(digest^bytes[j])*1099511628211ULL;
    }
    if(n<0 || count!=size)printf("SNAPPROBE IMAGE_READ ERROR errno=%d bytes=%lld\n",errno,count);
    else {printf("SNAPPROBE IMAGE_READ OK bytes=%lld diagnostic_fnv64=%016llx\n",count,(unsigned long long)digest);passed=1;}
free_image:
    if(ioctl(fd,SNAPSHOT_FREE,0)<0){printf("SNAPPROBE FREE ERROR %d\n",errno);passed=0;}else printf("SNAPPROBE FREE OK\n");
unfreeze:
    if(ioctl(fd,SNAPSHOT_UNFREEZE,0)<0){printf("SNAPPROBE UNFREEZE ERROR %d\n",errno);passed=0;}else printf("SNAPPROBE UNFREEZE OK\n");
close_fd:
    if(close(fd)<0){printf("SNAPPROBE CLOSE ERROR %d\n",errno);passed=0;}else printf("SNAPPROBE CLOSE OK\n");
done:
    printf("SNAPPROBE RESULT %s\n",passed?"PASS":"FAIL");
    printf("SNAPPROBE COMPLETE capture-discard-v1\n");
    sleep(2);reboot(RB_POWER_OFF);for(;;)pause();
}
