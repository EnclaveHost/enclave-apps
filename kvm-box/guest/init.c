#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/reboot.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <errno.h>
int main(void){
 mount("proc","/proc","proc",0,0);mount("sysfs","/sys","sysfs",0,0);mount("devtmpfs","/dev","devtmpfs",0,0);
 int console=open("/dev/console",O_RDWR);if(console>=0){dup2(console,0);dup2(console,1);dup2(console,2);if(console>2)close(console);}
 mount("tmpfs","/tmp","tmpfs",0,"size=256m,mode=1777");mount("tmpfs","/run","tmpfs",0,"size=64m,mode=0755");
 mkdir("/dev/shm",01777);mount("tmpfs","/dev/shm","tmpfs",0,"size=256m,mode=1777");
 mkdir("/dev/pts",0755);mount("devpts","/dev/pts","devpts",0,"mode=0620,ptmxmode=0666");
 mount("configfs","/sys/kernel/config","configfs",0,0);
 FILE *f=fopen("/module-order.txt","r");char path[512];if(!f){perror("module list");return 1;}
 while(fgets(path,sizeof(path),f)){path[strcspn(path,"\n")]=0;int fd=open(path,O_RDONLY|O_CLOEXEC);if(fd<0||syscall(SYS_finit_module,fd,"",4)<0){printf("MODULE %s: %s\n",path,strerror(errno));}if(fd>=0)close(fd);}fclose(f);
 pid_t child=fork();if(child==0){char *env[]={"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin","HOME=/root",NULL};char *argv[]={"/bin/sh","/start.sh",NULL};execve(argv[0],argv,env);perror("start");_exit(127);}
 printf("\nKVMBOX guest start pid=%d\n",child);fflush(stdout);
 for(;;){int status;pid_t p=wait(&status);if(p==child||p<0)break;}
 printf("KVMBOX guest stopping\n");fflush(stdout);sync();reboot(RB_POWER_OFF);return 0;
}
