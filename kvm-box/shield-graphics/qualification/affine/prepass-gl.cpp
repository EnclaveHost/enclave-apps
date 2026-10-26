#define GL_GLEXT_PROTOTYPES
#include <GL/gl.h>
#include <GL/glext.h>
#include <GL/glx.h>
#include <GL/glxext.h>
#include <X11/Xlib.h>
#include <array>
#include <vector>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <sstream>
#include <string>
static void require(bool ok,const char *why){if(!ok){std::fprintf(stderr,"%s\n",why);std::exit(1);}}
static void check(){require(glGetError()==GL_NO_ERROR,"OpenGL error");}
int main(int argc,char **argv){
 require(argc==2 || (argc==3 && (!std::strcmp(argv[2],"--expect-device-lost") || !std::strcmp(argv[2],"--queries") || !std::strcmp(argv[2],"--wide-range"))),"usage: prepass-gl SHADER [--expect-device-lost]");
 const bool expect_lost=argc==3 && !std::strcmp(argv[2],"--expect-device-lost");
 const bool query_mode=argc==3 && !std::strcmp(argv[2],"--queries"), wide_mode=argc==3 && !std::strcmp(argv[2],"--wide-range");
 Display *display=XOpenDisplay(nullptr);require(display,"no display");
 const int config_attr[]{GLX_X_RENDERABLE,True,GLX_DRAWABLE_TYPE,GLX_PBUFFER_BIT,GLX_RENDER_TYPE,GLX_RGBA_BIT,GLX_RED_SIZE,8,GLX_GREEN_SIZE,8,GLX_BLUE_SIZE,8,None};
 int count=0;GLXFBConfig *configs=glXChooseFBConfig(display,DefaultScreen(display),config_attr,&count);require(configs&&count,"no GLX config");
 auto create=reinterpret_cast<PFNGLXCREATECONTEXTATTRIBSARBPROC>(glXGetProcAddressARB(reinterpret_cast<const GLubyte*>("glXCreateContextAttribsARB")));require(create,"no core-context function");
 const int ctx_attr[]{GLX_CONTEXT_MAJOR_VERSION_ARB,4,GLX_CONTEXT_MINOR_VERSION_ARB,5,GLX_CONTEXT_PROFILE_MASK_ARB,GLX_CONTEXT_CORE_PROFILE_BIT_ARB,GLX_CONTEXT_FLAGS_ARB,expect_lost?GLX_CONTEXT_ROBUST_ACCESS_BIT_ARB:0,GLX_CONTEXT_RESET_NOTIFICATION_STRATEGY_ARB,GLX_LOSE_CONTEXT_ON_RESET_ARB,None};
 GLXContext ctx=create(display,configs[0],nullptr,True,ctx_attr);require(ctx,"no OpenGL 4.5 context");
 const int surface_attr[]{GLX_PBUFFER_WIDTH,32,GLX_PBUFFER_HEIGHT,32,None};GLXPbuffer surface=glXCreatePbuffer(display,configs[0],surface_attr);XFree(configs);require(surface,"no pbuffer");
 require(glXMakeContextCurrent(display,surface,surface,ctx),"context bind failed");
 const char *renderer=reinterpret_cast<const char*>(glGetString(GL_RENDERER));require(renderer&&std::strstr(renderer,"zink")&&std::strstr(renderer,"llvmpipe"),"not pinned software Vulkan through Zink");
 std::ifstream file(argv[1]);std::stringstream text;text<<file.rdbuf();std::string code=text.str();require(!code.empty(),"empty shader");const char *ptr=code.c_str();
 GLuint shader=glCreateShader(GL_COMPUTE_SHADER);glShaderSource(shader,1,&ptr,nullptr);glCompileShader(shader);GLint ok=0;glGetShaderiv(shader,GL_COMPILE_STATUS,&ok);
 if(!ok){char log[8192];glGetShaderInfoLog(shader,sizeof log,nullptr,log);std::fprintf(stderr,"%s\n",log);}require(ok,"compile failed");
 GLuint program=glCreateProgram();glAttachShader(program,shader);glLinkProgram(program);glGetProgramiv(program,GL_LINK_STATUS,&ok);require(ok,"link failed");glUseProgram(program);
 GLint alignment=0;glGetIntegerv(GL_SHADER_STORAGE_BUFFER_OFFSET_ALIGNMENT,&alignment);require(alignment>0&&alignment<=4096,"invalid buffer alignment");const size_t guard=alignment,size=1024;
 GLuint buffers[2];glGenBuffers(2,buffers);glBindBuffer(GL_SHADER_STORAGE_BUFFER,buffers[0]);glBufferData(GL_SHADER_STORAGE_BUFFER,size,nullptr,GL_DYNAMIC_DRAW);glBindBufferRange(GL_SHADER_STORAGE_BUFFER,0,buffers[0],0,size);
 glBindBuffer(GL_SHADER_STORAGE_BUFFER,buffers[1]);glBufferData(GL_SHADER_STORAGE_BUFFER,size+2*guard,nullptr,GL_DYNAMIC_DRAW);glBindBufferRange(GL_SHADER_STORAGE_BUFFER,1,buffers[1],guard,wide_mode?size+guard:size);
 GLuint query=0; if(query_mode)glGenQueries(1,&query);
 for(unsigned test=0;test<8;test++){
  std::array<uint32_t,256> input{};uint32_t rng=0x9e3779b9u+test;for(auto &word:input){rng^=rng<<13;rng^=rng>>17;rng^=rng<<5;word=rng;}input[0]=0;input[1]=255;input[2]=0xffffff00u;input[3]=0xffffffffu;
  std::vector<unsigned char> output(size+2*guard,0xa5);glBindBuffer(GL_SHADER_STORAGE_BUFFER,buffers[0]);glBufferSubData(GL_SHADER_STORAGE_BUFFER,0,size,input.data());glBindBuffer(GL_SHADER_STORAGE_BUFFER,buffers[1]);glBufferSubData(GL_SHADER_STORAGE_BUFFER,0,output.size(),output.data());check();
  if(query_mode)glBeginQuery(GL_COMPUTE_SHADER_INVOCATIONS_ARB,query);
  glDispatchCompute(1,1,1);
  if(query_mode)glEndQuery(GL_COMPUTE_SHADER_INVOCATIONS_ARB);
  glMemoryBarrier(GL_BUFFER_UPDATE_BARRIER_BIT);
  if(expect_lost){
   glFinish();
   GLenum reset=glGetGraphicsResetStatus();
   glFinish(); // A command after reset notification must hit context-lost dispatch.
   GLenum error=glGetError();
   std::fprintf(stderr,"OpenGL failure status reset=%u error=%u\n",reset,error);
   require(reset==GL_GUILTY_CONTEXT_RESET || reset==GL_INNOCENT_CONTEXT_RESET || reset==GL_UNKNOWN_CONTEXT_RESET,"driver did not report reset");
   require(error==GL_CONTEXT_LOST,"driver did not report context loss");
   std::printf("{\"passed\":true,\"api\":\"OpenGL\",\"contextLost\":true,\"readbackSkipped\":true}\n");
   return 0;
  }
  glGetBufferSubData(GL_SHADER_STORAGE_BUFFER,0,output.size(),output.data());check();
  if(query_mode){GLuint64 invocations=0;glGetQueryObjectui64v(query,GL_QUERY_RESULT,&invocations);require(invocations==64,"wrong compute invocation count");}
  for(size_t i=0;i<guard;i++)require(output[i]==0xa5&&output[guard+size+i]==0xa5,"guard overwritten");
  for(unsigned row=0;row<64;row++){int32_t v[4],actual[4];for(unsigned c=0;c<4;c++)v[c]=int32_t(input[row*4+c]&255u)-128;const int64_t weights[4][4]={{1,1,1,-1},{-1,1,0,2},{0,0,2,-1},{0,0,1,1}}; const uint64_t bias[4]={7,4294967293ull,2147483647ull,2147483648ull}, row_term[4]={1,4294967295ull,4294967295ull,2147483648ull}; uint32_t want[4];for(unsigned c=0;c<4;c++){int64_t dot=0;for(unsigned k=0;k<4;k++)dot+=int64_t(v[k])*weights[k][c];want[c]=uint32_t(uint64_t(dot)+bias[c]+uint64_t(row)*row_term[c]);}std::memcpy(actual,output.data()+guard+16*row,16);require(!std::memcmp(actual,want,16),"GL compute output differs from reference");}
  std::array<uint32_t,256> readback{};glBindBuffer(GL_SHADER_STORAGE_BUFFER,buffers[0]);glGetBufferSubData(GL_SHADER_STORAGE_BUFFER,0,size,readback.data());require(readback==input,"input changed");glBindBuffer(GL_SHADER_STORAGE_BUFFER,buffers[1]);check();
 }
 std::printf("{\"passed\":true,\"api\":\"OpenGL\",\"dispatches\":8,\"outputsChecked\":2048,\"guardsIntact\":true,\"gpuOffloadClaim\":false,\"renderer\":\"%s\"}\n",renderer);
 glDeleteBuffers(2,buffers);glDeleteProgram(program);glDeleteShader(shader);glXMakeContextCurrent(display,None,None,nullptr);glXDestroyPbuffer(display,surface);glXDestroyContext(display,ctx);XCloseDisplay(display);
}
