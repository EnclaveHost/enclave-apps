#!/usr/bin/env python3
"""Independent bounds expectations for global/local/workgroup affine proof.

Uses a built patched spirv2nir and installed glslangValidator/spirv-val. Records
compiler versions and hashes. This validates proof extraction, not GPU routing.
"""
import argparse,hashlib,json,re,subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--tool',required=True,type=Path)
p.add_argument('--output',required=True,type=Path)
a=p.parse_args();out=a.output.resolve();out.mkdir();tool=a.tool.resolve()
base=(Path(__file__).parent/'linear-prepass.comp').read_text()
fixed={'matrix':[[1,-1,0,0],[1,1,0,0],[0,0,1,0],[0,0,0,1]],'bias':[0]*4,'row':[0]*4}
cases={}
for name,row,groups in [
 ('global','gl_GlobalInvocationID.x',64),
 ('explicit-global','64u*gl_WorkGroupID.x+gl_LocalInvocationID.x',64),
 ('shift-global','(gl_WorkGroupID.x<<6)+gl_LocalInvocationID.x',64),
 ('local-only','gl_LocalInvocationID.x',1),
 ('workgroup-only','64u*gl_WorkGroupID.x',0),
 ('shift-boundary-valid','(gl_GlobalInvocationID.x<<20)>>20',64),
 ('shift-boundary-wrap','(gl_GlobalInvocationID.x<<21)>>21',1),
 ('wrong-group-stride32','32u*gl_WorkGroupID.x+gl_LocalInvocationID.x',1),
 ('wrong-group-stride128','128u*gl_WorkGroupID.x+gl_LocalInvocationID.x',1),
 ('wrapped-local','(gl_GlobalInvocationID.x<<26)>>26',1),
 ('global-mask4095','gl_GlobalInvocationID.x&4095u',64),
 ('local-mask63','gl_GlobalInvocationID.x&63u',1),
 ('missing-low-bit','gl_GlobalInvocationID.x&62u',0),
 ('nondivisible','((gl_GlobalInvocationID.x<<2)+1u)>>2',0),
 ('wrong-global-axis','gl_GlobalInvocationID.y',0),
]:
 cases[name]=(base.replace('gl_GlobalInvocationID.x',row),groups,fixed if groups else None)
# Global addresses cannot conceal local-only or workgroup-dependent row terms.
for name,term,groups,coefficient in [
 ('global-row-bias','gl_GlobalInvocationID.x*7u',64,7),
 ('explicit-row-bias','(gl_WorkGroupID.x*64u+gl_LocalInvocationID.x)*7u',64,7),
 ('local-row-bias','gl_LocalInvocationID.x*7u',1,7),
 ('group-row-bias','gl_WorkGroupID.x*7u',1,0),
 ('wrapped-row-bias','gl_GlobalInvocationID.x*0xffffffffu',64,0xffffffff),
 ('modular-local-row-bias','gl_LocalInvocationID.x*0x04000000u',64,0x04000000),
 ('modular-global-row-bias','gl_GlobalInvocationID.x*0x04000000u',64,0x04000000),
]:
 expected=json.loads(json.dumps(fixed));expected['row'][0]=coefficient
 cases[name]=(base.replace('v.x + v.y','int(uint(v.x+v.y)+'+term+')'),groups,expected)
cases['mixed-local-input']=(base.replace('source.words[4 * row','source.words[4 * gl_LocalInvocationID.x'),1,fixed)
cases['mixed-global-input']=(base.replace('uint row = gl_GlobalInvocationID.x','uint row = gl_LocalInvocationID.x').replace('source.words[4 * row','source.words[4 * gl_GlobalInvocationID.x'),1,fixed)
# Generate a scalar-store descriptor array form independently for every case.
for name,(shader,groups,expected) in list(cases.items()):
 z=shader.replace('layout(set = 0, binding = 0, std430) readonly buffer Input { uint words[]; } source;','layout(set=3,binding=0,std430) buffer Data { uint words[]; } buffers[2];')
 z=z.replace('layout(set = 0, binding = 1, std430) writeonly buffer Output { ivec4 positions[]; } destination;','')
 z=z.replace('source.words','buffers[0].words').replace('destination.positions[row] =','ivec4 result =')
 z=z.replace('\n}', '\n buffers[1].words[row*4]=uint(result.x);\n buffers[1].words[row*4+1]=uint(result.y);\n buffers[1].words[row*4+2]=uint(result.z);\n buffers[1].words[row*4+3]=uint(result.w);\n}')
 cases['zink-'+name]=(z,groups,expected)
results=[]
for name,(shader,want,expected) in cases.items():
 path=out/(name+'.comp');path.write_text(shader);spv=path.with_suffix('.spv')
 subprocess.run(['glslangValidator','-V','--target-env','vulkan1.1',str(path),'-o',str(spv)],check=True,stdout=subprocess.DEVNULL,timeout=15)
 subprocess.run(['spirv-val','--target-env','vulkan1.1',str(spv)],check=True,timeout=15)
 run=subprocess.run([str(tool),'--stage','cs','--optimize',str(spv)],capture_output=True,text=True,timeout=15,check=True)
 path.with_suffix('.proof').write_text(run.stderr)
 bound=re.findall(r'^SHIELD_MAX_WORKGROUPS=(\d+)$',run.stderr,re.M)
 transform=re.findall(r'^SHIELD_AFFINE64=(.+)$',run.stderr,re.M)
 assert len(bound)==1,(name,run.stderr)
 actual=int(bound[0]);matrix=json.loads(transform[0]) if len(transform)==1 else None
 passed=actual==want and matrix==expected
 results.append({'name':name,'passed':passed,'expectedMaxWorkgroups':want,'actualMaxWorkgroups':actual,'transform':matrix,'spirvSha256':hashlib.sha256(spv.read_bytes()).hexdigest()})
record={'passed':all(r['passed'] for r in results),'cases':results,'toolSha256':hashlib.sha256(tool.read_bytes()).hexdigest(),'glslangVersion':subprocess.check_output(['glslangValidator','--version'],text=True),'scope':'conditional proof bounds; not runtime or GPU qualification'}
(out/'results.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps({'passed':record['passed'],'cases':len(results),'failed':[r for r in results if not r['passed']]}))
assert record['passed']
