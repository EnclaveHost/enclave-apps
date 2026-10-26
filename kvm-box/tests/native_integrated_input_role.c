/* Test-only fixed input probe/denial entry, run inside the real nsinit root. */
#define _GNU_SOURCE
#include "role-policy.h"
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/utsname.h>
int main(int argc,char **argv){
 struct utsname u;if(argc!=2||getuid()||geteuid()||uname(&u)||strcmp(u.release,"6.6.87.2-enclave-kvm-box"))return 64;
 int input=!strcmp(argv[1],"input");uid_t uid=input?1000:!strcmp(argv[1],"deny-service")?1001:!strcmp(argv[1],"deny-ui")?1003:0;
 if(!uid)return 64;
 char device[128]={0};
 if(input){int f=open("/run/kvm-box-native-input-probe",O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW,0444);if(f<0)return 65;const char marker[]="INPUT-PROBE/1\n";if(write(f,marker,sizeof marker-1)!=sizeof marker-1)return 65;close(f);}
 else {int f=open("/tmp/native-input-device",O_RDONLY|O_NOFOLLOW|O_CLOEXEC);if(f<0)return 65;ssize_t n=read(f,device,sizeof device-1);close(f);if(n<=16||n>=127||strncmp(device,"/dev/input/event",16))return 65;for(ssize_t i=16;i<n;i++)if(device[i]<'0'||device[i]>'9')return 65;}
 if(syscall(SYS_close_range,3U,~0U,0)||native_enter_role(uid))return 66;
 if(!input){int f=open(device,O_RDONLY|O_NONBLOCK|O_NOFOLLOW|O_CLOEXEC);if(f>=0){close(f);return 67;}if(errno!=EACCES)return 68;dprintf(1,"NATIVE_INPUT_ROLE_DENIED uid=%u\n",uid);return 0;}
 char *const a[]={"/usr/bin/python3","-I","/probe-native-input.py",NULL};
 char *const e[]={"PATH=/usr/bin:/bin","HOME=/home/desktop","DISPLAY=:0","XDG_RUNTIME_DIR=/run/user/1000","XAUTHORITY=/run/user/1000/Xauthority","LANG=C.UTF-8",NULL};execve(a[0],a,e);return 127;
}
