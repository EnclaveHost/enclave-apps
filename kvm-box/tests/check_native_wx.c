/* Run only in a disposable child: the real inherited filter cannot be removed. */
#define _GNU_SOURCE
#include "../guest/native/wx-policy.h"
#include <pthread.h>
#include <signal.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/socket.h>
#include <sys/wait.h>
#define CHECK(x) do { if (!(x)) { fprintf(stderr,"FAIL line %d errno %d\n",__LINE__,errno); return 1; } } while (0)
#define DENIED(x) do { errno=0; long result=(long)(x); CHECK(result == -1 && errno == EPERM); } while (0)
static void *thread_probe(void *arg) {
    errno=0;
    void *p=mmap(NULL,4096,PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    *(int *)arg=(p==MAP_FAILED && errno==EPERM);
    if(p!=MAP_FAILED)munmap(p,4096);
    return NULL;
}
static int filtered(void) {
    CHECK(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0)==0);
    CHECK(native_seccomp_install()==0);
    DENIED(mmap(NULL,4096,PROT_READ|PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0));
    DENIED(mmap(NULL,4096,PROT_WRITE|PROT_EXEC,MAP_PRIVATE|MAP_ANONYMOUS,-1,0));
    DENIED(syscall(SYS_mmap,0,4096,PROT_WRITE|PROT_EXEC|0x1000000,MAP_PRIVATE|MAP_ANONYMOUS,-1,0));
    unsigned char *p=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    CHECK(p!=MAP_FAILED);
    memcpy(p,"\xb8\x2a\x00\x00\x00\xc3",6);
    DENIED(mprotect(p,4096,PROT_READ|PROT_WRITE|PROT_EXEC));
    p[1]=42; /* Failed mprotect did not remove the original writable permission. */
    CHECK(mprotect(p,4096,PROT_READ|PROT_EXEC)==0);
    CHECK(((int (*)(void))p)()==42);
    DENIED(mprotect(p,4096,PROT_READ|PROT_WRITE|PROT_EXEC));
    DENIED(syscall(SYS_pkey_mprotect,p,4096,PROT_READ|PROT_WRITE|PROT_EXEC,-1));
    CHECK(((int (*)(void))p)()==42); /* Failed upgrade preserved RX and code. */
    CHECK(mprotect(p,4096,PROT_READ|PROT_WRITE)==0);
    p[1]=43;
    CHECK(mprotect(p,4096,PROT_READ|PROT_EXEC)==0 && ((int (*)(void))p)()==43);
    CHECK(munmap(p,4096)==0);
    long personality=syscall(SYS_personality,0xffffffffUL);
    CHECK(personality>=0);
    DENIED(syscall(SYS_personality,personality|NWX_READ_IMPLIES_EXEC));
    CHECK(syscall(SYS_personality,0xffffffffUL)==personality);
    int shmid=shmget(IPC_PRIVATE,4096,IPC_CREAT|0700);
    CHECK(shmid>=0);
    char *shared=shmat(shmid,NULL,0);
    CHECK(shared!=(void *)-1); shared[0]=17;
    CHECK(shmdt(shared)==0);
    DENIED(shmat(shmid,NULL,SHM_EXEC));
    shared=shmat(shmid,NULL,SHM_EXEC|SHM_RDONLY);
    CHECK(shared!=(void *)-1 && shared[0]==17);
    CHECK(shmdt(shared)==0 && shmctl(shmid,IPC_RMID,NULL)==0);
    int inherited=0; pthread_t thread;
    CHECK(pthread_create(&thread,NULL,thread_probe,&inherited)==0);
    CHECK(pthread_join(thread,NULL)==0 && inherited);
    pid_t child=fork(); CHECK(child>=0);
    if(!child){int ok=0;thread_probe(&ok);_exit(ok?23:24);}
    int status;CHECK(waitpid(child,&status,0)==child && WIFEXITED(status) && WEXITSTATUS(status)==23);
    child=fork(); CHECK(child>=0);
    if(!child){execl("/proc/self/exe","check-native-wx","--exec-probe",(char *)NULL);_exit(25);}
    CHECK(waitpid(child,&status,0)==child && WIFEXITED(status) && WEXITSTATUS(status)==0);
    /* A failed descendant did not terminate its parent; existing denials stay. */
    DENIED(socket(40 /* AF_VSOCK */,SOCK_STREAM,0));
    DENIED(syscall(SYS_ptrace,0,0,0,0));
    int pair[2]; CHECK(socketpair(AF_UNIX,SOCK_STREAM,0,pair)==0);
    CHECK(write(pair[0],"ok",2)==2);char buf[2];CHECK(read(pair[1],buf,2)==2 && !memcmp(buf,"ok",2));
    close(pair[0]);close(pair[1]);
    puts("PASS: deny RWX mmap/mprotect/pkey/shmat and implicit-exec; preserve RW/RX JIT, threads, fork/exec, IPC and existing isolation");
    return 0;
}
int main(int argc, char **argv) {
    if(argc==2 && !strcmp(argv[1],"--exec-probe")) {
        int inherited=0; thread_probe(&inherited); CHECK(inherited);
        DENIED(socket(40 /* AF_VSOCK */,SOCK_STREAM,0));
        return 0;
    }
    char statement[160]; CHECK(native_seccomp_statement(statement,sizeof statement)>0);
    fputs(statement,stdout);fflush(stdout);
    pid_t child=fork();CHECK(child>=0);
    if(!child){int result=filtered();fflush(stdout);_exit(result);}
    int status;CHECK(waitpid(child,&status,0)==child);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status)==0);
    /* The test runner and siblings never inherit the child's filter. */
    return 0;
}
