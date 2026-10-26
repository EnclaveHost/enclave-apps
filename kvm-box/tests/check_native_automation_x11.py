#!/usr/bin/env python3
"""Test native capture against an isolated authenticated Xvfb, never the user's X display."""
from pathlib import Path
import argparse,hashlib
args=argparse.ArgumentParser()
args.add_argument('--xvfb',required=True,type=Path)
args.add_argument('--capture',required=True,type=Path)
args.add_argument('--output',required=True,type=Path)
opts=args.parse_args()
import os,subprocess,time,json,struct,secrets,socket,ctypes
w=opts.output.resolve();w.mkdir(parents=True,exist_ok=False)
if os.getuid()!=1000:raise RuntimeError('production capture helper requires UID 1000')
for display in range(177,191):
 if not Path(f'/tmp/.X{display}-lock').exists() and not Path(f'/tmp/.X11-unix/X{display}').exists(): break
else: raise RuntimeError('no unused test display')
auth=w/'test.xauthority';cookie=secrets.token_bytes(16)
def field(b):return struct.pack('>H',len(b))+b
auth.write_bytes(struct.pack('>H',256)+field(socket.gethostname().encode())+field(str(display).encode())+field(b'MIT-MAGIC-COOKIE-1')+field(cookie));auth.chmod(0o600)
env={'PATH':'/usr/bin:/bin','LANG':'C.UTF-8','DISPLAY':f':{display}','XAUTHORITY':str(auth)}
with (w/'xvfb.log').open('w') as log:
 p=subprocess.Popen([str(opts.xvfb.resolve()),f':{display}','-screen','0','960x600x24','-nolisten','tcp','-auth',str(auth),'-noreset'],stdout=log,stderr=log,env=env)
 try:
  for _ in range(50):
   if p.poll() is not None:raise RuntimeError('test X server exited')
   r=subprocess.run(['xdotool','getdisplaygeometry'],env=env,capture_output=True)
   if r.returncode==0:break
   time.sleep(.1)
  assert r.stdout.strip()==b'960 600',r.stdout
  lib=ctypes.CDLL('libX11.so.6')
  lib.XOpenDisplay.argtypes=[ctypes.c_char_p];lib.XOpenDisplay.restype=ctypes.c_void_p
  lib.XDefaultRootWindow.argtypes=[ctypes.c_void_p];lib.XDefaultRootWindow.restype=ctypes.c_ulong
  lib.XSetWindowBackground.argtypes=[ctypes.c_void_p,ctypes.c_ulong,ctypes.c_ulong]
  lib.XClearWindow.argtypes=[ctypes.c_void_p,ctypes.c_ulong]
  lib.XSync.argtypes=[ctypes.c_void_p,ctypes.c_int]
  lib.XCloseDisplay.argtypes=[ctypes.c_void_p]
  os.environ['XAUTHORITY']=str(auth)
  d=lib.XOpenDisplay(env['DISPLAY'].encode());assert d
  root=lib.XDefaultRootWindow(d)
  lib.XSetWindowBackground(d,root,0xde1e0a);lib.XClearWindow(d,root);lib.XSync(d,0)
  subprocess.run(['xdotool','mousemove','480','300'],env=env,check=True,timeout=3)
  cap=opts.capture.resolve()
  b=subprocess.check_output([str(cap)],env=env,timeout=3)
  assert b[:4]==b'RGB1'
  dims=struct.unpack('>IIII',b[4:20]);assert dims==(960,600,480,300),dims
  assert len(b)==20+960*600*3
  assert b[20:23]==bytes([222,30,10]),b[20:23]
  # Confirm an option-like string remains literal typed input, not a filename.
  typed=subprocess.run(['xdotool','type','--clearmodifiers','--delay','0','--','--file /nonexistent-test-file; $(echo harmless)'],env=env,capture_output=True,timeout=3)
  assert typed.returncode==0,typed.stderr
  # Same-coordinate move must not wait for a coordinate change.
  subprocess.run(['xdotool','mousemove','480','300'],env=env,check=True,timeout=3)
  report={'status':'passed','display':display,'width':dims[0],'height':dims[1],'pointer':[dims[2],dims[3]],'firstPixel':list(b[20:23]),'bytes':len(b),'typedOptionLikeText':True,'sameCoordinateMove':True,'productionVMChanged':False,'captureSha256':hashlib.sha256(cap.read_bytes()).hexdigest(),'xvfbSha256':hashlib.sha256(opts.xvfb.read_bytes()).hexdigest()}
  (w/'live-x11-result.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))
 finally:
  if 'd' in locals() and d:lib.XCloseDisplay(d)
  p.terminate()
  try:p.wait(timeout=5)
  except subprocess.TimeoutExpired:p.kill();p.wait()
  auth.unlink(missing_ok=True)
