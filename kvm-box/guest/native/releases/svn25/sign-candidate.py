from pathlib import Path
import hashlib,json,os,resource,stat,subprocess
resource.setrlimit(resource.RLIMIT_CORE,(0,0))
h=Path(__file__).resolve().parent;w=h.parent;b=w.parent
record=json.loads((h/'igvm-build.json').read_text());assert record['candidate']['deterministic'] and all(x['changed'] for x in record['mutations'])
image=Path(record['kept'])/'vbs-linux-candidate.bin';tool=b/'outputs/kvm-box/evidence/native-image-signer-v1/enclave-vbs-image-sign'
def sha(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
assert sha(tool)=='64a9ebffd4da42f3d30c800d3de84483b67d5ebdf5e06b90c14e125ee0559412'
assert sha(image)==record['candidate']['binSha']
identity=json.loads((b/'outputs/kvm-box/native-platform-v4-signed-graphics/candidate.json').read_text())['imageIdentity']
key=w/'native-desktop-release-key/key.pkcs8.pem';st=key.lstat();assert stat.S_ISREG(st.st_mode) and stat.S_IMODE(st.st_mode)==0o600 and st.st_uid==os.getuid()
cmd=[str(tool),'sign','--input',str(image),'--expected-input-sha256',record['candidate']['binSha'],'--expected-signer',identity['signer'],'--product-id',str(identity['productId']),'--module-id',str(identity['moduleId']),'--svn','25','--output',str(h/'signed-desktop')]
# Dedicated candidate key is passed only as stdin to the pinned signer.
with key.open('rb') as stdin:
 result=subprocess.run(cmd,stdin=stdin,capture_output=True,text=True)
(h/'sign-result.json').write_text(json.dumps({'command':cmd,'returncode':result.returncode,'stdout':result.stdout,'stderr':result.stderr},indent=2)+'\n');assert result.returncode==0,result.stderr
manifest=json.loads((h/'signed-desktop/manifest.json').read_text());assert manifest['launchMeasurement'].lower()==record['candidate']['digest'].lower()
verify=[str(tool),'verify','--input',str(h/'signed-desktop/image.igvm'),'--expected-input-sha256',manifest['signedImageSha256'],'--expected-measurement',record['candidate']['digest'].lower(),'--expected-signer',identity['signer'],'--product-id',str(identity['productId']),'--module-id',str(identity['moduleId']),'--minimum-svn','25']
r=subprocess.run(verify,capture_output=True,text=True);(h/'signature-verification.json').write_text(json.dumps({'command':verify,'returncode':r.returncode,'stdout':r.stdout,'stderr':r.stderr},indent=2)+'\n');assert r.returncode==0,r.stderr
print(json.dumps({'signedImageSha256':manifest['signedImageSha256'],'launchMeasurement':manifest['launchMeasurement'],'identity':identity,'localSignatureVerified':True,'productionAdmission':False}))
