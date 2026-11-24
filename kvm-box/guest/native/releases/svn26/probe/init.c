/* Disposable KVM fixture only: authenticate root bytes with a build-pinned
 * dm-verity hash, then run the real image's desktop graph. No host devices. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h>
#include <sys/ioctl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>
#include "verity-params.h"
#define CHECK(x) do {if(!(x)){dprintf(2,"IMAGE_PROBE_FAIL line=%d errno=%d\n",__LINE__,errno);return 90;}}while(0)
static int md(const char *p,mode_t mode){return mkdir(p,mode)==0||errno==EEXIST?0:-1;}
static int run_cmd(char *const a[]){pid_t c=fork();if(!c){execv(a[0],a);dprintf(2,"IMAGE_EXEC_FAIL errno=%d\n",errno);_exit(127);}if(c<0)return -1;int st;while(waitpid(c,&st,0)<0)if(errno!=EINTR)return -1;return WIFEXITED(st)?WEXITSTATUS(st):128;}
static int bind_file(const char *src,const char *dst){int f=open(dst,O_CREAT|O_WRONLY|O_EXCL,0555);if(f<0)return -1;close(f);if(mount(src,dst,NULL,MS_BIND,NULL))return -1;return mount(NULL,dst,NULL,MS_REMOUNT|MS_BIND|MS_RDONLY|MS_NOSUID|MS_NODEV,NULL);}
static int bind_dir(const char *src,const char *dst){if(md(dst,0755))return -1;if(mount(src,dst,NULL,MS_BIND,NULL))return -1;return mount(NULL,dst,NULL,MS_REMOUNT|MS_BIND|MS_RDONLY|MS_NOSUID|MS_NODEV,NULL);}
static int run(void){
 CHECK(mount("devtmpfs","/dev","devtmpfs",MS_NOSUID,"mode=0755")==0);
 CHECK(mount("proc","/proc","proc",MS_NOSUID|MS_NODEV|MS_NOEXEC,"hidepid=2")==0);
 CHECK(mount("sysfs","/sys","sysfs",MS_RDONLY|MS_NOSUID|MS_NODEV|MS_NOEXEC,NULL)==0);
 CHECK(mount("tmpfs","/run","tmpfs",MS_NOSUID|MS_NODEV,"mode=0755,size=32m")==0);
 CHECK(md("/dev/input",0755)==0);
 for(int i=0;i<100 && access("/dev/sdc",F_OK);i++)usleep(100000);
 CHECK(access("/dev/sdc",F_OK)==0);
 char *const verity[]={"/veritysetup","open","--no-superblock","--format=1","--hash=sha256","--hash-offset=0","--data-block-size=4096","--hash-block-size=4096","--data-blocks=" DATA_BLOCKS,"--salt=" VERITY_SALT,"/dev/sda","native-root","/dev/sdb",VERITY_ROOT,NULL};
 CHECK(run_cmd(verity)==0);
 int fd=open("/dev/mapper/native-root",O_RDONLY|O_CLOEXEC);CHECK(fd>=0);
 int readonly=0;CHECK(ioctl(fd,BLKROGET,&readonly)==0 && readonly==1);
 char data[65536];ssize_t got;unsigned long long total=0;int err=0;
 while((got=read(fd,data,sizeof data))>0)total+=(unsigned long long)got;
 if(got<0)err=errno;close(fd);
 char cmdline[4096]={0};fd=open("/proc/cmdline",O_RDONLY);CHECK(fd>=0);CHECK(read(fd,cmdline,sizeof cmdline-1)>0);close(fd);
 if(strstr(cmdline,"probe.expect-root-corruption=1")){
  CHECK(got<0 && err==EIO);
  dprintf(1,"IMAGE_VERITY_RESULT={\"passed\":true,\"corruptionRefused\":true,\"errno\":%d,\"bytesRead\":%llu}\n",err,total);return 0;
 }
 CHECK(got==0 && total==IMAGE_BYTES);
 CHECK(md("/plat",0755)==0 && md("/plat/native-desktop",0755)==0 && md("/plat/native-desktop/rootfs",0755)==0);
 CHECK(mount("/dev/mapper/native-root","/plat/native-desktop/rootfs","squashfs",MS_RDONLY|MS_NOSUID|MS_NODEV,NULL)==0);
 dprintf(1,"IMAGE_VERITY_RESULT={\"passed\":true,\"fullImageRead\":true,\"readOnly\":true,\"bytesRead\":%llu}\n",total);
 const char *root="/plat/native-desktop/rootfs";char path[256];
 for(int i=0;i<4;i++){const char *dirs[]={"usr","etc","opt","lib64"};snprintf(path,sizeof path,"%s/%s",root,dirs[i]);char dst[64];snprintf(dst,sizeof dst,"/%s",dirs[i]);CHECK(bind_dir(path,dst)==0);}
 const char *names[]={"nsinit","supervisor","gateway","shield-desktop.wasm"};
 const char *dests[]={"/plat/native-desktop/nsinit","/supervisor","/gateway","/shield-desktop.wasm"};
 for(int i=0;i<4;i++){snprintf(path,sizeof path,"%s/usr/local/libexec/kvm-box/%s",root,names[i]);CHECK(bind_file(path,dests[i])==0);}
 CHECK(bind_file("/plat/native-desktop/rootfs/desktop.profile","/desktop.profile")==0);
 char *const ip[]={"/bin/busybox","ip","link","set","lo","up",NULL};CHECK(run_cmd(ip)==0);
 char *const insmod[]={"/bin/busybox","insmod","/plat/native-desktop/rootfs/usr/local/libexec/kvm-box/nft_ct.ko",NULL};CHECK(run_cmd(insmod)==0);
 char *const nft[]={"/usr/sbin/nft","-f","/probe-network.nft",NULL};CHECK(run_cmd(nft)==0);
 CHECK(md("/run/native-desktop",0750)==0 && chown("/run/native-desktop",0,1002)==0);
 CHECK(md("/run/native-desktop/models",0755)==0);
 char *const modelverity[]={"/plat/native-desktop/rootfs/shield/mverity","native-models","/dev/sdc","4096","4096",MODEL_BLOCKS,MODEL_HASH_START,"sha256",MODEL_ROOT,MODEL_SALT,NULL};
 CHECK(run_cmd(modelverity)==0);
 fd=open("/dev/mapper/native-models",O_RDONLY|O_CLOEXEC);CHECK(fd>=0);
 readonly=0;CHECK(ioctl(fd,BLKROGET,&readonly)==0 && readonly==1);total=0;err=0;
 while((got=read(fd,data,sizeof data))>0)total+=(unsigned long long)got;
 if(got<0)err=errno;close(fd);
 if(strstr(cmdline,"probe.expect-model-corruption=1")){
  CHECK(got<0 && err==EIO);
  dprintf(1,"MODEL_VERITY_RESULT={\"passed\":true,\"corruptionRefused\":true,\"errno\":%d,\"bytesRead\":%llu}\n",err,total);return 0;
 }
 CHECK(got==0 && total==MODEL_BYTES);
 CHECK(mount("/dev/mapper/native-models","/run/native-desktop/models","ext4",MS_RDONLY|MS_NOSUID|MS_NODEV|MS_NOEXEC,"noload")==0);
 dprintf(1,"MODEL_VERITY_RESULT={\"passed\":true,\"fullImageRead\":true,\"readOnly\":true,\"bytesRead\":%llu}\n",total);
 struct sockaddr_un a={.sun_family=AF_UNIX};strcpy(a.sun_path,"/run/native-desktop/worker.sock");
 int worker=socket(AF_UNIX,SOCK_STREAM|SOCK_CLOEXEC|SOCK_NONBLOCK,0);CHECK(worker>=0);
 CHECK(bind(worker,(struct sockaddr*)&a,sizeof a)==0 && listen(worker,32)==0 && chown(a.sun_path,0,1001)==0 && chmod(a.sun_path,0660)==0);
 CHECK(dup2(worker,3)==3 && fcntl(3,F_SETFD,0)==0);
 CHECK(syscall(SYS_close_range,4U,~0U,0)==0);
 char *const av[]={"/usr/bin/python3","-I","/probe-chain.py",NULL};char *const env[]={"PATH=/usr/sbin:/usr/bin:/sbin:/bin","LANG=C.UTF-8",NULL};execve(av[0],av,env);return 95;
}
int main(void){struct utsname u;if(getpid()!=1||getuid()||uname(&u)||strcmp(u.release,"6.6.87.2-enclave-kvm-box"))return 64;int r=run();dprintf(1,"IMAGE_PROBE_EXIT=%d\n",r);sync();reboot(RB_POWER_OFF);return r;}
