/* Disposable KVM test harness. Synthetic models and local worker only.
 * No production admission, network device, host input, GPU or shared mounts. */
#define _GNU_SOURCE
#include "native-root.h"
#include <sched.h>
#include <signal.h>
#include <poll.h>
#include <stdlib.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#define CHECK(x) do { if (!(x)) { dprintf(2,"NSINIT_PROBE_FAIL line=%d errno=%d\n",__LINE__,errno); return 90; } } while (0)
static int md(const char *p, mode_t m) { return mkdir(p,m)==0 || errno==EEXIST ? 0 : -1; }
static int put(const char *p,const char *s) { int f=open(p,O_WRONLY|O_CREAT|O_EXCL,0444); if(f<0)return -1; int r=write(f,s,strlen(s))==(ssize_t)strlen(s)?0:-1; close(f);return r; }
static int run_cmd(char *const a[]) { pid_t c=fork(); if(!c){execv(a[0],a);_exit(127);} if(c<0)return -1;int st;while(waitpid(c,&st,0)<0){if(errno!=EINTR)return -1;}return WIFEXITED(st)?WEXITSTATUS(st):128; }
static int run(void) {
 CHECK(mount("devtmpfs","/dev","devtmpfs",MS_NOSUID,"mode=0755")==0);
 CHECK(mount("proc","/proc","proc",MS_NOSUID|MS_NODEV|MS_NOEXEC,"hidepid=2")==0);
 CHECK(mount("sysfs","/sys","sysfs",MS_RDONLY|MS_NOSUID|MS_NODEV|MS_NOEXEC,NULL)==0);
 CHECK(mount("tmpfs","/run","tmpfs",MS_NOSUID|MS_NODEV,"mode=0755,size=32m")==0);
 CHECK(md("/dev/input",0755)==0);
 char *const ip[]={"/bin/busybox","ip","link","set","lo","up",NULL}; CHECK(run_cmd(ip)==0);
 char *const insmod[]={"/bin/busybox","insmod","/probe-nft-ct.ko",NULL};CHECK(run_cmd(insmod)==0);
 char *const nft[]={"/usr/sbin/nft","-f","/probe-network.nft",NULL}; CHECK(run_cmd(nft)==0);
 CHECK(md("/plat",0755)==0 && md("/plat/native-desktop",0755)==0 && md("/plat/native-desktop/rootfs",0755)==0 && md("/models",0755)==0);
 CHECK(chown("/",0,0)==0 && chmod("/",0755)==0);
 CHECK(mount("/","/plat/native-desktop/rootfs",NULL,MS_BIND,NULL)==0);
 CHECK(mount(NULL,"/plat/native-desktop/rootfs",NULL,MS_REMOUNT|MS_BIND|MS_RDONLY|MS_NOSUID|MS_NODEV,NULL)==0);
 CHECK(mkdir("/run/native-desktop",0750)==0 && chown("/run/native-desktop",0,1002)==0);
 CHECK(mkdir("/run/native-desktop/models",0755)==0);
 CHECK(mount("tmpfs","/run/native-desktop/models","tmpfs",MS_NOSUID|MS_NODEV,"mode=0755")==0);
 CHECK(put("/run/native-desktop/models/fixture","SYNTHETIC MODEL FOR LIFECYCLE TEST ONLY\n")==0);
 CHECK(mount(NULL,"/run/native-desktop/models",NULL,MS_REMOUNT|MS_RDONLY|MS_NOSUID|MS_NODEV,NULL)==0);
 struct sockaddr_un a={.sun_family=AF_UNIX}; strcpy(a.sun_path,"/run/native-desktop/worker.sock");
 int worker=socket(AF_UNIX,SOCK_STREAM|SOCK_CLOEXEC|SOCK_NONBLOCK,0);CHECK(worker>=0);
 CHECK(bind(worker,(struct sockaddr*)&a,sizeof a)==0 && listen(worker,32)==0 && chown(a.sun_path,0,1001)==0 && chmod(a.sun_path,0660)==0);
 CHECK(dup2(worker,3)==3 && fcntl(3,F_SETFD,0)==0);
 CHECK(syscall(SYS_close_range,4U,~0U,0)==0);
 char *const av[]={"/usr/bin/python3","-I","/probe-chain.py",NULL};
 char *const env[]={"PATH=/usr/sbin:/usr/bin:/sbin:/bin","LANG=C.UTF-8",NULL};
 execve(av[0],av,env);return 95;
}
int main(void){struct utsname u;if(getpid()!=1||getuid()||uname(&u)||strcmp(u.release,NR_KERNEL))return 64;int r=run();dprintf(1,"NSINIT_PROBE_EXIT=%d\n",r);sync();reboot(RB_POWER_OFF);return r;}
