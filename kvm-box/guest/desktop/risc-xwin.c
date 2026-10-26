/* Tiny Xlib window activator for the emulated RISC Box desktop.
 * Uses the window manager's client list; no Python startup or tree walk.
 * Xlib ABI declarations avoid requiring development headers in the guest. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>
#include <dirent.h>
#include <sys/resource.h>
typedef struct _XDisplay Display;
typedef unsigned long Window, Atom;
typedef struct { int type; unsigned long serial; int send_event; Display *display; Window window; Atom message_type; int format; union { char b[20]; short s[10]; long l[5]; } data; } XClientMessageEvent;
typedef union { int type; XClientMessageEvent xclient; long pad[24]; } XEvent;
extern Display *XOpenDisplay(const char *);
extern Window XDefaultRootWindow(Display *);
extern int XDefaultScreen(Display *), XCloseDisplay(Display *), XFree(void *);
extern Atom XInternAtom(Display *, const char *, int);
extern int XGetWindowProperty(Display *,Window,Atom,long,long,int,Atom,Atom*,int*,unsigned long*,unsigned long*,unsigned char**);
extern int XIconifyWindow(Display *,Window,int), XMapRaised(Display *,Window), XSync(Display *,int);
extern int XSendEvent(Display *,Window,int,long,XEvent *);
extern int XSetErrorHandler(int (*)(Display *,void *));
static int ignore_error(Display *d,void *e) { (void)d; (void)e; return 0; }
static unsigned char *property(Display *d,Window w,const char *name,int *format,unsigned long *count) {
    Atom type; unsigned long after; unsigned char *data=NULL;
    if(XGetWindowProperty(d,w,XInternAtom(d,name,0),0,4096,0,0,&type,format,count,&after,&data)!=0) { if(data)XFree(data); return NULL; }
    return data;
}
/* Extension startup must not compete at equal priority with typing. */
static void background_extensions(void) {
    DIR *proc=opendir("/proc"); if(!proc)return;
    struct dirent *e;
    while((e=readdir(proc))) {
        if(!isdigit((unsigned char)e->d_name[0]))continue;
        char path[512], name[64];
        snprintf(path,sizeof(path),"/proc/%s/comm",e->d_name);
        FILE *f=fopen(path,"r");if(!f)continue;
        int match=fgets(name,sizeof(name),f)&&!strcmp(name,"WebExtensions\n");
        fclose(f);if(!match)continue;
        snprintf(path,sizeof(path),"/proc/%s/task",e->d_name);
        DIR *tasks=opendir(path);if(!tasks)continue;
        struct dirent *t;
        while((t=readdir(tasks))) {
            if(isdigit((unsigned char)t->d_name[0]))setpriority(PRIO_PROCESS,atoi(t->d_name),10);
        }
        closedir(tasks);
    }
    closedir(proc);
}
int main(int argc,char **argv) {
    const char *cmd=argc>1?argv[1]:"list",*cls=argc>2?argv[2]:"firefox";
    if(!strcmp(cmd,"priorities")){background_extensions();return 0;}
    if(!strcmp(cmd,"show"))background_extensions();
    if(strcmp(cmd,"list")&&strcmp(cmd,"show")&&strcmp(cmd,"hide")) return 2;
    Display *d=XOpenDisplay(NULL); if(!d){fputs("no display\n",stderr);return 2;}
    XSetErrorHandler(ignore_error);
    Window root=XDefaultRootWindow(d); int fmt=0,found=0; unsigned long n=0;
    unsigned char *clients=property(d,root,"_NET_CLIENT_LIST",&fmt,&n);
    if(clients&&fmt==32) for(unsigned long i=0;i<n;i++) {
        Window w=((unsigned long*)clients)[i]; int f=0;unsigned long len=0;
        unsigned char *wc=property(d,w,"WM_CLASS",&f,&len); int match=0;
        if(wc&&f==8) { for(unsigned long j=0;j<len;j++) wc[j]=wc[j]?(unsigned char)tolower(wc[j]):' '; match=strstr((char*)wc,cls)!=NULL; }
        if(wc)XFree(wc);
        if(!match)continue;
        found++;
        if(!strcmp(cmd,"hide")) XIconifyWindow(d,w,XDefaultScreen(d));
        if(!strcmp(cmd,"show")) {
            XMapRaised(d,w); XEvent ev={0};ev.xclient.type=33;ev.xclient.window=w;
            ev.xclient.message_type=XInternAtom(d,"_NET_ACTIVE_WINDOW",0);ev.xclient.format=32;ev.xclient.data.l[0]=2;
            XSendEvent(d,root,0,(1L<<20)|(1L<<19),&ev);
        }
        printf("0x%lx\n",w);
    }
    if(clients)XFree(clients);
    XSync(d,0);XCloseDisplay(d);return found?0:1;
}
