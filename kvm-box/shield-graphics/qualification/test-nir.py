from pathlib import Path
import subprocess,json,hashlib,os,re
import argparse
parser=argparse.ArgumentParser(description='Compile public semantic-admission fixtures with the pinned Mesa proof tool.')
parser.add_argument('--image',required=True)
parser.add_argument('--source',type=Path,required=True,help='Prepared Mesa source with driver and nir-proof-tool patches')
parser.add_argument('--build',type=Path,required=True,help='Build parent containing the configured mesa/ build')
parser.add_argument('--output',type=Path,required=True,help='Fresh output directory')
args=parser.parse_args()
assert re.fullmatch(r'sha256:[0-9a-f]{64}',args.image)
h=args.output.resolve();h.mkdir();out=h/'fixtures';out.mkdir()
s=(Path(__file__).resolve().parent/'linear-prepass.comp').read_text()
variants={
 'original':(s,True),
 'renamed':(s.replace('positions','transformed').replace('row','item').replace('source','input_data'),True),
 'reassociated':(s.replace('v.x + v.y, v.y - v.x','v.y + v.x, -v.x + v.y'),True),
 'explicit-scalar':(s.replace('ivec4(v.x + v.y, v.y - v.x, v.z, v.w)','ivec4((v.y+v.x)+0, v.y+(-v.x), v.z*1, v.w-0)'),True),
 'wrong-matrix':(s.replace('v.x + v.y','v.x - v.y'),True),
 'wrong-mask':(s.replace('255u','254u'),False),
 'wrong-centering':(s.replace('ivec4(128)','ivec4(127)'),True),
 'wrong-row':(s.replace('gl_GlobalInvocationID.x','gl_GlobalInvocationID.y'),False),
 'wrong-binding':(s.replace('binding = 0','binding = 3'),False),
 'wrong-workgroup':(s.replace('local_size_x = 64','local_size_x = 32'),False),
 'wrong-output-offset':(s.replace('ivec4 positions[]','layout(offset=16) ivec4 positions[]'),False),
 'extra-write':(s.replace('    destination.positions[row] =', '    destination.positions[row + 1] = ivec4(0);\n    destination.positions[row] ='),False),
 'private-branch':(s.replace('    destination.positions[row] =','    if ((source.words[0] & 1u) != 0u)\n    destination.positions[row] ='),False),
 'unclamped':(s.replace(' & 255u',''),False),
 'fixed-short-input':(s.replace('uint words[]','uint words[4]'),False),
 'fixed-short-output':(s.replace('ivec4 positions[]','ivec4 positions[4]'),False),
 'volatile':(s.replace('readonly buffer Input','volatile readonly buffer Input'),False),
}
zink = """#version 450
layout(local_size_x=64) in;
layout(set=3,binding=0,std430) buffer Data { uint words[]; } buffers[2];
void main() {
 uint row=gl_WorkGroupID.x*64+gl_LocalInvocationID.x;
 uint offset=(row<<4)>>2;
 ivec4 v=ivec4(buffers[0].words[offset]&255u,buffers[0].words[offset+1]&255u,buffers[0].words[offset+2]&255u,buffers[0].words[offset+3]&255u)-ivec4(128);
 buffers[1].words[offset]=uint(v.x+v.y);
 buffers[1].words[offset+1]=uint(v.y-v.x);
 buffers[1].words[offset+2]=uint(v.z);
 buffers[1].words[offset+3]=uint(v.w);
}
"""
variants.update({
 'zink-original':(zink,True),
 'zink-local-id':(zink.replace('gl_WorkGroupID.x*64+gl_LocalInvocationID.x','gl_LocalInvocationID.x'),True),
 'zink-renamed':(zink.replace('buffers','storage').replace('offset','address'),True),
 'zink-wrong-set':(zink.replace('set=3','set=2'),False),
 'zink-wrong-binding':(zink.replace('binding=0','binding=1'),False),
 'zink-wrong-input':(zink.replace('buffers[0].words','buffers[1].words'),False),
 'zink-wrong-output':(zink.replace('buffers[1].words','buffers[0].words'),False),
 'zink-missing-store':(zink.replace(' buffers[1].words[offset+3]=uint(v.w);',''),False),
 'zink-wrong-component':(zink.replace('uint(v.z)','uint(v.w)'),True),
 'zink-wrong-store-index':(zink.replace('words[offset+3]=','words[offset+4]='),False),
 'zink-modular-equivalent':(zink.replace('(row<<4)>>2','((row+0x10000000u)<<4)>>2'),True),
 'zink-shift-overflow':(zink.replace('(row<<4)>>2','(row*0x40000010u)>>2'),False),
 'zink-shift-private':(zink.replace('(row<<4)>>2','(buffers[0].words[0]<<4)>>2'),False),
 'zink-shift-nondivisible':(zink.replace('(row<<4)>>2','((row<<4)+1)>>2'),False),
 'zink-atomic':(zink.replace(' buffers[1].words[offset+3]=uint(v.w);',' atomicAdd(buffers[1].words[offset+3],uint(v.w));'),False),
})
for name,(text,expected) in variants.items():(out/(name+'.comp')).write_text(text)
(h/'expectations.json').write_text(json.dumps({k:v[1] for k,v in variants.items()},indent=2)+'\n')
image=args.image;exp=args.build.resolve();mesa_source=args.source.resolve()
script='''set -eu
ninja -C /build/mesa -j2 src/compiler/spirv/spirv2nir
for shader in /fixtures/*.comp; do
 glslangValidator -V --target-env vulkan1.1 "$shader" -o "$shader.spv"
 spirv-val --target-env vulkan1.1 "$shader.spv"
 /build/mesa/src/compiler/spirv/spirv2nir --stage cs --optimize "$shader.spv" > "$shader.nir" 2> "$shader.proof"
done
'''
cmd=['docker','run','--rm','--init','--network','none','--cpus','2','--memory','2g','--user',f'{os.getuid()}:{os.getgid()}','-v',str(mesa_source)+':/source:ro','-v',str(exp)+':/build:rw','-v',str(out)+':/fixtures:rw',image,'sh','-c',script]
with (h/'structural-tests.log').open('w') as log:p=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT,timeout=60)
assert p.returncode==0
reasons={'wrong-mask': 'expression', 'wrong-row': 'input-access', 'wrong-binding': 'input-access', 'wrong-workgroup': 'metadata', 'wrong-output-offset': 'output-layout', 'extra-write': 'output-layout', 'private-branch': 'control-flow', 'unclamped': 'expression', 'fixed-short-input': 'input-access', 'fixed-short-output': 'output-layout', 'volatile': 'volatile-access', 'zink-wrong-set': 'input-access', 'zink-wrong-binding': 'input-access', 'zink-wrong-input': 'input-access', 'zink-wrong-output': 'output-layout', 'zink-missing-store': 'store-shape', 'zink-wrong-store-index': 'output-layout', 'zink-shift-overflow': 'expression', 'zink-shift-private': 'input-access', 'zink-shift-nondivisible': 'expression', 'zink-atomic': 'intrinsic'}
results=[]
for name,(_,expected) in variants.items():
 report=(out/(name+'.comp.proof')).read_text();tokens=re.findall(r'^SHIELD_STRUCTURAL_PROOF=(accepted|rejected) reason=([a-z-]+) contract=([01234])$',report,re.M);assert len(tokens)==1,(name,'missing/ambiguous proof result');actual=tokens[0][0]=='accepted';assert (int(tokens[0][2]) in [1,2,3,4])==actual;reason=tokens[0][1]
 result={'name':name,'expected':expected,'accepted':actual,'reason':reason,'contract':int(tokens[0][2]),'expectedReason':reasons.get(name,'ok'),'passed':actual==expected and reason==reasons.get(name,'ok') and (not actual or int(tokens[0][2])==(4 if name=='zink-wrong-component' else 3 if name in ['wrong-matrix','wrong-centering'] else 2 if name.startswith('zink-') else 1)),'spirvSha256':hashlib.sha256((out/(name+'.comp.spv')).read_bytes()).hexdigest()};results.append(result)
record={'passed':all(r['passed'] for r in results),'cases':results,'scope':'Compiler admission tests; GPU routing is measured separately by driver tests'};(h/'structural-results.json').write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(record));assert record['passed']
