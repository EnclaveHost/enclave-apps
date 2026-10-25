from pathlib import Path
import hashlib, json, os, random, re, subprocess

import argparse
p=argparse.ArgumentParser(description='Extract bounded affine transforms from both NIR descriptor layouts.')
p.add_argument('--image',required=True);p.add_argument('--source',type=Path,required=True)
p.add_argument('--build',type=Path,required=True);p.add_argument('--output',type=Path,required=True)
a=p.parse_args();assert re.fullmatch(r'sha256:[0-9a-f]{64}',a.image)
out=a.output.resolve();out.mkdir();fixtures=out/'fixtures';fixtures.mkdir()
repo=Path(__file__).resolve().parent
base=(repo/'linear-prepass.comp').read_text()
old='ivec4(v.x + v.y, v.y - v.x, v.z, v.w)'
assert old in base
rng=random.Random(0x8164a11)
cases={}
def expression(matrix,bias,row):
    components=[]
    for c in range(4):
        terms=[f'uint(v.{"xyzw"[k]})*{matrix[k][c] & 0xffffffff}u' for k in range(4)]
        terms.extend([f'{bias[c]}u',f'row*{row[c]}u'])
        components.append('int('+ '+'.join(terms)+')')
    return 'ivec4('+','.join(components)+')'
def add(name,matrix,bias,row):
    shader=base.replace(old,expression(matrix,bias,row))
    expected={'matrix':matrix,'bias':bias,'row':row}
    cases[name]=(shader,expected)
for i in range(12):
    matrix=[[rng.randrange(-119,120) for _ in range(4)] for _ in range(4)]
    bias=[rng.randrange(1<<32) for _ in range(4)];row=[rng.randrange(1<<32) for _ in range(4)]
    add(f'random-{i:02}',matrix,bias,row)
for value in [-119,-1,1,119]:add(f'boundary-{value}',[[value]*4 for _ in range(4)],[0,0x80000000,0xffffffff,17],[0xffffffff,0,0x80000000,13])
fixed=[[1,-1,0,0],[1,1,0,0],[0,0,1,0],[0,0,0,1]]
for value in [-120,120,-2147483648,2147483647]:
    m=[row[:] for row in fixed];m[0][0]=value
    cases[f'reject-weight-{value}']=(base.replace(old,expression(m,[0]*4,[0]*4)),None)
cases['reject-nonlinear']=(base.replace(old,'ivec4(v.x*v.y,v.y-v.x,v.z,v.w)'),None)
cases['reject-float']=(base.replace(old,'ivec4(int(float(v.x)*1.5),v.y-v.x,v.z,v.w)'),None)
cases['reject-extra-input']=(base.replace('source.words[4 * row]','source.words[4 * row + 4]'),None)
# Generate an independent Zink-layout scalar store shader for each admitted matrix.
for name,(text,expected) in list(cases.items()):
    if expected is None:continue
    m,b,r=expected['matrix'],expected['bias'],expected['row']
    vector=expression(m,b,r)
    text='''#version 450
layout(local_size_x=64) in;
layout(set=3,binding=0,std430) buffer Data { uint words[]; } buffers[2];
void main() {
 uint row=gl_WorkGroupID.x*64+gl_LocalInvocationID.x;
 uint offset=(row<<4)>>2;
 ivec4 v=ivec4(buffers[0].words[offset]&255u,buffers[0].words[offset+1]&255u,buffers[0].words[offset+2]&255u,buffers[0].words[offset+3]&255u)-ivec4(128);
 ivec4 result=VECTOR;
 buffers[1].words[offset]=uint(result.x);
 buffers[1].words[offset+1]=uint(result.y);
 buffers[1].words[offset+2]=uint(result.z);
 buffers[1].words[offset+3]=uint(result.w);
}
'''.replace('VECTOR',vector)
    cases['zink-'+name]=(text,expected)
cases['zink-reject-positive-weight']=(cases['zink-boundary-119'][0].replace('uint(v.x)*119u','uint(v.x)*120u',1),None)
cases['zink-reject-negative-weight']=(cases['zink-boundary--119'][0].replace('uint(v.x)*4294967177u','uint(v.x)*4294967176u',1),None)
for name,(shader,expected) in cases.items():
    assert shader!=base,(name,'fixture replacement did not change the baseline')
    (fixtures/(name+'.comp')).write_text(shader)
script='''set -eu
ninja -C /build/mesa -j2 src/compiler/spirv/spirv2nir
for shader in /fixtures/*.comp; do
 glslangValidator -V --target-env vulkan1.1 "$shader" -o "$shader.spv"
 spirv-val --target-env vulkan1.1 "$shader.spv"
 /build/mesa/src/compiler/spirv/spirv2nir --stage cs --optimize "$shader.spv" > "$shader.nir" 2> "$shader.proof"
done
'''
cmd=['docker','run','--rm','--init','--network','none','--cpus','2','--memory','2g','--user',f'{os.getuid()}:{os.getgid()}',
     '-v',str(a.source.resolve())+':/source:ro','-v',str(a.build.resolve())+':/build:rw','-v',str(fixtures)+':/fixtures:rw',
     a.image,'sh','-c',script]
with (out/'build.log').open('w') as log:subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT,check=True,timeout=120)
results=[]
for name,(shader,expected) in cases.items():
    report=(fixtures/(name+'.comp.proof')).read_text()
    headers=re.findall(r'^SHIELD_STRUCTURAL_PROOF=(accepted|rejected) reason=([a-z-]+) contract=([01234])$',report,re.M)
    assert len(headers)==1,(name,report)
    transforms=re.findall(r'^SHIELD_AFFINE64=(.+)$',report,re.M)
    actual=json.loads(transforms[0]) if len(transforms)==1 else None
    contract=int(headers[0][2]);passed=(actual==expected and headers[0][0]==('accepted' if expected else 'rejected') and
        contract==((4 if name.startswith('zink-') else 3) if expected else 0))
    results.append({'name':name,'passed':passed,'expected':expected,'actual':actual,'contract':contract,'reason':headers[0][1],
                    'spirvSha256':hashlib.sha256((fixtures/(name+'.comp.spv')).read_bytes()).hexdigest()})
record={'passed':all(r['passed'] for r in results),'cases':results,'scope':'Offline matrix/bias/row extraction; no driver routing or GPU execution claim'}
(out/'results.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps({'passed':record['passed'],'cases':len(results),'failed':[r for r in results if not r['passed']]}))
assert record['passed']
