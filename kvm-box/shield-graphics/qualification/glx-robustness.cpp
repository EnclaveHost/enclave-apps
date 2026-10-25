#define GL_GLEXT_PROTOTYPES
#include <GL/gl.h>
#include <GL/glext.h>
#include <GL/glx.h>
#include <GL/glxext.h>
#include <X11/Xlib.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
static void check(bool ok,const char*message){if(!ok){std::fprintf(stderr,"%s\n",message);std::exit(1);}}
static bool robust(const char *s){return s&&std::strstr(s,"GLX_ARB_create_context_robustness");}
static int xerror=0;
static int on_error(Display*,XErrorEvent*e){xerror=e->error_code;return 0;}
int main(){
 Display*d=XOpenDisplay(nullptr);check(d,"No display");int screen=DefaultScreen(d);
 bool server=robust(glXQueryServerString(d,screen,GLX_EXTENSIONS)),client=robust(glXGetClientString(d,GLX_EXTENSIONS)),combined=robust(glXQueryExtensionsString(d,screen));
 const int config[]{GLX_X_RENDERABLE,True,GLX_DRAWABLE_TYPE,GLX_PBUFFER_BIT,GLX_RENDER_TYPE,GLX_RGBA_BIT,GLX_RED_SIZE,8,GLX_GREEN_SIZE,8,GLX_BLUE_SIZE,8,None};int n=0;GLXFBConfig*cfg=glXChooseFBConfig(d,screen,config,&n);check(cfg&&n,"No config");
 auto create=reinterpret_cast<PFNGLXCREATECONTEXTATTRIBSARBPROC>(glXGetProcAddressARB(reinterpret_cast<const GLubyte*>("glXCreateContextAttribsARB")));check(create,"No createContext function");
 const int attrs[]{GLX_CONTEXT_MAJOR_VERSION_ARB,4,GLX_CONTEXT_MINOR_VERSION_ARB,5,GLX_CONTEXT_PROFILE_MASK_ARB,GLX_CONTEXT_CORE_PROFILE_BIT_ARB,GLX_CONTEXT_FLAGS_ARB,GLX_CONTEXT_ROBUST_ACCESS_BIT_ARB,GLX_CONTEXT_RESET_NOTIFICATION_STRATEGY_ARB,GLX_LOSE_CONTEXT_ON_RESET_ARB,None};
 GLXContext c=create(d,cfg[0],nullptr,True,attrs);check(c,"No robust context");const int pa[]{GLX_PBUFFER_WIDTH,32,GLX_PBUFFER_HEIGHT,32,None};GLXPbuffer pb=glXCreatePbuffer(d,cfg[0],pa);check(pb,"No pbuffer");check(glXMakeContextCurrent(d,pb,pb,c),"Cannot bind");
 GLint flags=0,strategy=0;glGetIntegerv(GL_CONTEXT_FLAGS,&flags);glGetIntegerv(GL_RESET_NOTIFICATION_STRATEGY,&strategy);check(glGetError()==GL_NO_ERROR,"GL query error");
 std::printf("{\"serverAdvertises\":%s,\"clientAdvertises\":%s,\"combinedAdvertises\":%s,\"afterContextAdvertises\":%s,\"robustFlag\":%s,\"loseContextOnReset\":%s,\"renderer\":\"%s\"}\n",server?"true":"false",client?"true":"false",combined?"true":"false",robust(glXQueryExtensionsString(d,screen))?"true":"false",(flags&GL_CONTEXT_FLAG_ROBUST_ACCESS_BIT)?"true":"false",strategy==GL_LOSE_CONTEXT_ON_RESET?"true":"false",glGetString(GL_RENDERER));

 check(combined,"Robustness not discoverable through the ordinary extension query");
 check(glXIsDirect(d,c),"Expected a direct context");
 check(flags&GL_CONTEXT_FLAG_ROBUST_ACCESS_BIT,"Robust flag missing");
 check(strategy==GL_LOSE_CONTEXT_ON_RESET,"Reset strategy missing");
 auto previous=XSetErrorHandler(on_error);
 auto mismatch=[&](int reset,GLXContext shared,int expected){
  const int bad[]{GLX_CONTEXT_MAJOR_VERSION_ARB,4,GLX_CONTEXT_MINOR_VERSION_ARB,5,GLX_CONTEXT_PROFILE_MASK_ARB,GLX_CONTEXT_CORE_PROFILE_BIT_ARB,GLX_CONTEXT_RESET_NOTIFICATION_STRATEGY_ARB,reset,None};
  XSync(d,False);xerror=0;auto invalid=create(d,cfg[0],shared,True,bad);XSync(d,False);
  if(invalid)glXDestroyContext(d,invalid);
  std::printf("{\"reset\":%d,\"error\":%d,\"expected\":%d,\"rejected\":%s}\n",reset,xerror,expected,invalid?"false":"true");
  check(!invalid&&xerror==expected,"Invalid reset/share context was not rejected correctly");
 };
 mismatch(GLX_NO_RESET_NOTIFICATION_ARB,c,BadMatch);
 mismatch(0xdead,nullptr,BadMatch);
 XSetErrorHandler(previous);
 glXMakeContextCurrent(d,None,None,nullptr);glXDestroyPbuffer(d,pb);glXDestroyContext(d,c);XFree(cfg);XCloseDisplay(d);
}
