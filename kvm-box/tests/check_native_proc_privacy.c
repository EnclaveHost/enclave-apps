#define _GNU_SOURCE
#include "proc-privacy.h"
#include <assert.h>
static void check(const char *text, uint64_t id, int want, int expected_found) {
 char copy[2048];assert(strlen(text)<sizeof copy);strcpy(copy,text);int found=0;
 assert((np_mount_line(copy,id,&found)==0)==want);assert(found==expected_found);
}
int main(void){
 check("42 1 0:3 / /proc rw,nosuid,nodev,noexec - proc proc rw,hidepid=invisible\n",42,1,1);
 check("42 1 0:3 / /proc rw - proc proc rw,hidepid=2,gid=0\n",42,1,1);
 check("42 1 0:3 / /proc rw - proc proc rw\n",42,0,0);
 check("42 1 0:3 / /proc rw - proc proc rw,hidepid=1\n",42,0,0);
 check("42 1 0:3 / /proc rw - proc proc rw,hidepid=2,gid=1000\n",42,0,0);
 check("43 1 0:3 / /proc-old rw - proc proc rw\n",42,0,0);
 check("43 1 0:3 / /proc-old rw - proc proc rw,hidepid=2\n",42,1,0);
 check("42 1 0:3 / /proc-old rw - proc proc rw,hidepid=2\n",42,0,0);
 check("42 1 0:3 / /proc rw - tmpfs tmpfs rw\n",42,0,0);
 check("42 1 0:3 /1 /proc rw - proc proc rw,hidepid=2\n",42,0,0);
 check("42 1 0:3 / /proc rw - proc proc rw,hidepid=2,hidepid=0\n",42,0,0);
 check("42 1 0:3 / /proc rw - proc\n",42,0,0);
 check("99 1 0:4 / /tmp rw shared:2 - tmpfs tmpfs rw\n",42,1,0);
 puts("native ancestor proc mount parser: 13 cases passed");return 0;
}
