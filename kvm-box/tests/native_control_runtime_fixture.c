/* Test-only fixed Wasmtime entry. No endpoint/path/secret arguments. */
#define _GNU_SOURCE
#include "role-policy.h"
#include <fcntl.h>
int main(void){
 if(getuid()||geteuid())return 64;
 for(int c=0;c<64;c++){if(prctl(PR_CAPBSET_READ,c,0,0,0)<0){if(errno==EINVAL)break;return 65;}if(prctl(PR_CAPBSET_DROP,c,0,0,0))return 65;}
 if(prctl(PR_CAP_AMBIENT,PR_CAP_AMBIENT_CLEAR_ALL,0,0,0)||setgroups(0,NULL)||setresgid(7501,7501,7501)||setresuid(7501,7501,7501)||prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)||app_seccomp_install())return 66;
 if(syscall(SYS_close_range,3U,~0U,0))return 67;
 char *const a[]={"/probe-wasmtime","run","-C","cache=n","-S","inherit-network=y","--env","KVMBOX_OWNER_PASSWORD","/shield-desktop.wasm",NULL};
 char *const e[]={"PATH=/usr/bin:/bin","HOME=/nonexistent","LANG=C.UTF-8","KVMBOX_OWNER_PASSWORD=synthetic-control-chain-owner",NULL};
 execve(a[0],a,e);return 127;
}
