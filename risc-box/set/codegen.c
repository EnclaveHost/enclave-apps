/* The optional enclave codegen ABI. Function pointers begin at local failure
 * stubs; codegen-componentize wires their table slots to the host functions.
 * Every SET execution view gets its own fixup and generated-function table. */
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <pthread.h>

static int64_t unavailable(uint64_t ptr, uint64_t len) { (void)ptr; (void)len; return -1; }
static int32_t unavailable_drop(uint64_t slot) { (void)slot; return 0; }
static int64_t (*volatile compile_fn)(uint64_t, uint64_t) = unavailable;
static int32_t (*volatile drop_fn)(uint64_t) = unavailable_drop;

__attribute__((export_name("__enclave_codegen_compile_slot")))
uintptr_t codegen_compile_slot(void) { return (uintptr_t)unavailable; }
__attribute__((export_name("__enclave_codegen_drop_slot")))
uintptr_t codegen_drop_slot(void) { return (uintptr_t)unavailable_drop; }

int64_t risc_codegen_compile(const uint8_t *bytes, size_t len) {
    return compile_fn((uintptr_t)bytes, len);
}
int32_t risc_codegen_drop(uint64_t slot) { return drop_fn(slot); }

/* A runtime-generated module, including the address of a real C stack cell.
 * This is exercised only with RISC_CODEGEN_SELFTEST=1; normal app startup does
 * not compile or run a synthetic workload. */
struct bytes { uint8_t b[256]; size_t n; };
static void byte(struct bytes *b, uint8_t v) { b->b[b->n++] = v; }
static void uleb(struct bytes *b, uint64_t v) {
    do { uint8_t p=v&127; v>>=7; byte(b,p|(v?128:0)); } while(v);
}
static void sleb(struct bytes *b, int64_t v) {
    for (;;) { uint8_t p=v&127; v>>=7; int done=(v==0&&!(p&64))||(v==-1&&(p&64)); byte(b,p|(done?0:128));if(done)break; }
}
static void name(struct bytes *b, const char *s, size_t n) { uleb(b,n);for(size_t i=0;i<n;i++)byte(b,s[i]); }
static void section(struct bytes *b, uint8_t id, struct bytes *s) {
    byte(b,id);uleb(b,s->n);for(size_t i=0;i<s->n;i++)byte(b,s->b[i]);s->n=0;
}
static void address(struct bytes *b, uintptr_t ptr) { byte(b,sizeof(uintptr_t)==8?0x42:0x41);sleb(b,(int64_t)ptr); }
static int check(const char *label) {
    volatile uint64_t cell=0;
    struct bytes m={{0},0},s={{0},0},body={{0},0};
    const uint8_t header[]={0,97,115,109,1,0,0,0};
    for(size_t i=0;i<sizeof(header);i++)byte(&m,header[i]);
    byte(&s,1);byte(&s,0x60);byte(&s,2);byte(&s,0x7e);byte(&s,0x7f);byte(&s,1);byte(&s,0x7e);section(&m,1,&s);
    byte(&s,1);name(&s,"env",3);name(&s,"memory",6);byte(&s,2);
    byte(&s,sizeof(uintptr_t)==8?7:3);uleb(&s,0);uleb(&s,sizeof(uintptr_t)==8?262144:65536);section(&m,2,&s);
    byte(&s,1);byte(&s,0);section(&m,3,&s);
    byte(&s,1);name(&s,"run",3);byte(&s,0);byte(&s,0);section(&m,7,&s);
    byte(&body,0);address(&body,(uintptr_t)&cell);
    byte(&body,0x20);byte(&body,0);byte(&body,0x20);byte(&body,1);byte(&body,0xac);byte(&body,0x7c);
    byte(&body,0x37);byte(&body,3);byte(&body,0);
    address(&body,(uintptr_t)&cell);byte(&body,0x29);byte(&body,3);byte(&body,0);byte(&body,0x0b);
    byte(&s,1);uleb(&s,body.n);for(size_t i=0;i<body.n;i++)byte(&s,body.b[i]);section(&m,10,&s);
    int64_t slot=risc_codegen_compile(m.b,m.n);
    if(slot<0) {fprintf(stderr,"codegen %s: compile failed status=%lld\n",label,(long long)slot);return 1;}
    int64_t (*run)(int64_t,int32_t)=(void *)(uintptr_t)slot;
    int64_t result=run(1234567890123LL,-123);
    int ok=result==1234567890000LL&&cell==1234567890000ULL;
    int dropped=risc_codegen_drop(slot), twice=risc_codegen_drop(slot);
    printf("codegen %s: slot=%lld result=%lld memory=%llu revoke=%d duplicate=%d %s\n",label,(long long)slot,(long long)result,(unsigned long long)cell,dropped,twice,ok&&dropped&&!twice?"PASS":"FAIL");
    return !(ok&&dropped&&!twice);
}
static void *worker(void *unused) { (void)unused;return (void *)(uintptr_t)check("SET worker"); }
int risc_codegen_selftest(void) {
    if(check("main")) return 1;
    pthread_t thread;
    int err=pthread_create(&thread,0,worker,0);
    if(err) {fprintf(stderr,"codegen: pthread_create failed %d\n",err);return 1;}
    void *result=0;err=pthread_join(thread,&result);
    if(err) {fprintf(stderr,"codegen: pthread_join failed %d\n",err);return 1;}
    return (int)(uintptr_t)result;
}
