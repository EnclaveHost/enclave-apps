#!/usr/bin/env python3
"""Build ordinary API fixtures and their separately selected CPU reference."""
from pathlib import Path
import argparse, hashlib, json, os, re, subprocess
p=argparse.ArgumentParser()
p.add_argument('--image',required=True)
p.add_argument('--reference',type=Path,required=True,help='Separate fixed-only or unmodified Mesa install prefix')
p.add_argument('--output',type=Path,required=True,help='Fresh qualification directory')
a=p.parse_args();assert re.fullmatch(r'sha256:[0-9a-f]{64}',a.image)
source=Path(__file__).resolve().parent;reference=a.reference.resolve();output=a.output.resolve()
assert (reference/'lib/libvulkan_lvp.so').is_file()
output.mkdir();output.chmod(0o777)
script='''set -eu
[ ! -e /dev/dri ] && [ ! -e /dev/nvidia0 ]
[ ! -e /run/shield/compute.sock ]
OUTPUT=/output/build sh /source/build.sh
OUTPUT=/output/gl sh /source/build-gl.sh
cp /output/gl/prepass-gl /output/gl/prepass-gl.comp /output/
export VK_DRIVER_FILES=/opt/shield/mesa/share/vulkan/icd.d/lvp_icd.x86_64.json
export LD_LIBRARY_PATH=/opt/shield/mesa/lib LIBGL_DRIVERS_PATH=/opt/shield/mesa/lib/dri
export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=zink MESA_LOADER_DRIVER_OVERRIDE=zink
export LIBGL_KOPPER_DRI2=true ZINK_DESCRIPTORS=lazy LP_NUM_THREADS=2 XDG_RUNTIME_DIR=/tmp
/output/build/prepass /output/build /output/triangle.rgba
xvfb-run -a /output/prepass-gl /output/prepass-gl.comp
'''
command=['docker','run','--rm','--init','--network','none','--read-only','--cpus','2','--memory','2g',
         '--user',f'{os.getuid()}:{os.getgid()}','--tmpfs','/tmp:rw,mode=1777',
         '-e','HOME=/tmp','-v',str(source)+':/source:ro','-v',str(reference)+':/opt/shield/mesa:ro',
         '-v',str(output)+':/output:rw',a.image,'sh','-c',script]
with (output/'prepare.log').open('x') as log:
    subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,check=True,timeout=120)
def sha(path):
    with path.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
record={'passed':True,'image':a.image,'referenceIcdSha256':sha(reference/'lib/libvulkan_lvp.so'),
        'files':{str(f.relative_to(output)):sha(f) for f in sorted(output.rglob('*')) if f.is_file()},
        'sources':{f.name:sha(f) for f in sorted(source.iterdir()) if f.is_file()},
        'scope':'CPU reference with no service socket, network or physical GPU device; does not test protected routing'}
(output/'prepare.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps({'passed':True,'output':str(output),'triangleSha256':record['files']['triangle.rgba']}))
