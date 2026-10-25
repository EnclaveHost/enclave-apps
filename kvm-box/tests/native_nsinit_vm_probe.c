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
static pid_t check_roles(pid_t init) {
 char path[128];snprintf(path,sizeof path,"/proc/%d/task/%d/children",init,init);
 FILE *f=fopen(path,"r");if(!f)return -1;
 pid_t p,flux=0;unsigned count=0,root=0;int valid=1;
 while(fscanf(f,"%d",&p)==1){
  char line[256],comm[64]={0};unsigned uid=9999,nnp=0,sec=0;unsigned caps=0;unsigned long long eff=~0ULL;
  snprintf(path,sizeof path,"/proc/%d/status",p);FILE *st=fopen(path,"r");if(!st){valid=0;break;}
  while(fgets(line,sizeof line,st)){
   unsigned long long v;
   if(sscanf(line,"Uid:\t%u",&uid)==1)continue;
   if(sscanf(line,"NoNewPrivs:\t%u",&nnp)==1)continue;
   if(sscanf(line,"Seccomp:\t%u",&sec)==1)continue;
   if(sscanf(line,"CapEff:\t%llx",&v)==1)eff=v;
   if(!strncmp(line,"Cap",3) && sscanf(line+7,"%llx",&v)==1){caps++;if(v && uid!=0)valid=0;}
  }
  fclose(st);
  if(uid==0){root++;if(eff!=0x1009)valid=0;}else if(uid!=1000 && uid!=1001 && uid!=1003)valid=0;
  if(nnp!=1 || sec!=2 || caps!=5)valid=0;
  snprintf(path,sizeof path,"/proc/%d/comm",p);st=fopen(path,"r");if(!st){valid=0;break;}if(!fgets(comm,sizeof comm,st))valid=0;fclose(st);
  if(!strcmp(comm,"fluxbox\n")){if(flux||uid!=1000)valid=0;flux=p;}
  count++;
 }
 fclose(f);
 dprintf(1,"NSINIT_ROLE_CHECK children=%u rootHelpers=%u valid=%d\n",count,root,valid);
 return valid && count==10 && root==1 && flux>0 ? flux : -1;
}
static int critical_case(void){char b[2048];int f=open("/proc/cmdline",O_RDONLY|O_CLOEXEC);if(f<0)return -1;ssize_t n=read(f,b,sizeof b-1);close(f);if(n<0)return -1;b[n]=0;return strstr(b,"nsinit_case=critical")!=NULL;}
static int run(void) {
 CHECK(mount("devtmpfs","/dev","devtmpfs",MS_NOSUID,"mode=0755")==0);
 CHECK(mount("proc","/proc","proc",MS_NOSUID|MS_NODEV|MS_NOEXEC,NULL)==0);
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
 int pf=(int)syscall(SYS_pidfd_open,getpid(),0);CHECK(pf>=0);int parent=fcntl(pf,F_DUPFD_CLOEXEC,20);close(pf);CHECK(parent>=0);
 int pp[2];CHECK(pipe2(pp,O_CLOEXEC|O_NONBLOCK)==0);int wr=fcntl(pp[1],F_DUPFD_CLOEXEC,30);CHECK(wr>=0);
 pid_t c=(pid_t)syscall(SYS_clone,CLONE_NEWPID|CLONE_NEWNS|CLONE_NEWIPC|CLONE_NEWUTS|SIGCHLD,0,0,0,0);CHECK(c>=0);
 if(!c){ if(mount(NULL,"/",NULL,MS_REC|MS_PRIVATE,NULL) || dup2(wr,3)!=3 || dup2(parent,4)!=4 || syscall(SYS_close_range,5U,~0U,0))_exit(91); char *const av[]={"/nsinit",NULL};char *const en[]={NULL};execve(av[0],av,en);_exit(92); }
 close(wr);close(pp[1]);close(parent);
 int ready=0,eof=0,exited=0,st=0,peers=0;unsigned ticks=0;
 while(ticks++<700 && !exited && !eof){
  struct pollfd p={.fd=pp[0],.events=POLLIN};CHECK(poll(&p,1,100)>=0);
  if(p.revents){char b[8];ssize_t n=read(pp[0],b,sizeof b);if(n==0)eof=1;else if(n>0){CHECK(n==1 && b[0]=='R' && !ready);ready=1;}else CHECK(errno==EAGAIN);}
  int s=accept4(worker,NULL,NULL,SOCK_CLOEXEC|SOCK_NONBLOCK);if(s>=0){struct ucred u;socklen_t len=sizeof u;CHECK(getsockopt(s,SOL_SOCKET,SO_PEERCRED,&u,&len)==0 && u.uid==1001);peers++;close(s);}else CHECK(errno==EAGAIN);
  pid_t w=waitpid(c,&st,WNOHANG);CHECK(w==0||w==c);exited=w==c;
 }
 dprintf(1,"NSINIT_STARTUP ready=%d eof=%d exited=%d status=%d workerPeers=%d\n",ready,eof,exited,st,peers);
 if(!ready || !eof || exited){if(!exited){kill(c,SIGKILL);waitpid(c,&st,0);}return 93;}
 CHECK(peers>=1);close(pp[0]);
 /* Probe the actual inner gateway through the intended UID, with production role policy. */
 char *const probe[]={"/probe-inner",NULL};CHECK(run_cmd(probe)==0);
 pid_t flux=check_roles(c);CHECK(flux>0);int critical=critical_case();CHECK(critical>=0);
 CHECK(kill(critical?flux:c,critical?SIGKILL:SIGTERM)==0);
 exited=0;for(unsigned i=0;i<45;i++){pid_t w=waitpid(c,&st,WNOHANG);CHECK(w==0||w==c);if(w==c){exited=1;break;}poll(NULL,0,100);}
 if(!exited){kill(c,SIGKILL);waitpid(c,&st,0);return 94;}
 CHECK(WIFEXITED(st)&&WEXITSTATUS(st)==(critical?1:0));
 CHECK(kill(flux,0)==-1 && errno==ESRCH);
 close(worker);
 dprintf(1,"NSINIT_LIFECYCLE_PASS realGraph=true syntheticResources=true case=%s\n",critical?"critical-daemon-failure":"clean-stop");return 0;
}
int main(void){struct utsname u;if(getpid()!=1||getuid()||uname(&u)||strcmp(u.release,NR_KERNEL))return 64;int r=run();dprintf(1,"NSINIT_PROBE_EXIT=%d\n",r);sync();reboot(RB_POWER_OFF);return r;}
