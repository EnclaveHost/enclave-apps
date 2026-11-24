from pathlib import Path
import hashlib,json,struct
from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import hashes,serialization
from cryptography.hazmat.primitives.asymmetric import ec,utils
h=Path(__file__).resolve().parent
signed=(h/'signed-desktop/image.igvm').read_bytes()
manifest=json.loads((h/'signed-desktop/manifest.json').read_text())
build=json.loads((h/'igvm-build.json').read_text())
assert hashlib.sha256(signed).hexdigest()==manifest['signedImageSha256']
start,size=struct.unpack_from('<II',signed,8);end=start+size;matches=[]
while start<end:
 typ,n=struct.unpack_from('<II',signed,start)
 if typ==0x311:matches.append(signed[start+8:start+8+n])
 start+=8+((n+7)//8)*8
assert start==end and len(matches)==1
header=matches[0];assert len(header)==864
assert struct.unpack_from('<8I',header)==(1,1,1818455653,1802724708,26,0,1,1)
assert header[32:64].hex()==build['candidate']['digest'].lower()==manifest['launchMeasurement']
assert header[64:96]==bytes(32) and header[192:352]==bytes(160) and header[456:864]==bytes(408)
public=serialization.load_pem_public_key((h.parent/'native-desktop-release-key/public.pem').read_bytes())
assert isinstance(public.curve,ec.SECP384R1)
p=public.public_numbers();cng=struct.pack('<II',0x33534345,48)+p.x.to_bytes(48,'big')+p.y.to_bytes(48,'big')
assert header[352:456]==cng==(h/'signed-desktop/public-cng.bin').read_bytes()
assert hashlib.sha256(cng).hexdigest()==manifest['identity']['signer']=='c47c23318e325d23a9c0110f6b6aa59d80cd8f24629b1a5fe77fbb6fb9c5e89d'
payload=header[4:64];sig=header[96:192]
der=utils.encode_dss_signature(int.from_bytes(sig[:48],'big'),int.from_bytes(sig[48:],'big'))
public.verify(der,payload,ec.ECDSA(hashes.SHA384()))
for message,algorithm in [(payload,hashes.SHA256()),(payload[:-1]+bytes([payload[-1]^1]),hashes.SHA384())]:
 try:public.verify(der,message,ec.ECDSA(algorithm))
 except InvalidSignature:pass
 else:raise AssertionError('Invalid signature accepted')
assert manifest['guestSvn']==26
result={'passed':True,'implementation':'Python cryptography independent of RustCrypto signer','signedImageSha256':manifest['signedImageSha256'],'launchMeasurement':manifest['launchMeasurement'],'identity':manifest['identity'],'guestSvn':manifest['guestSvn'],'sha384Verified':True,'wrongHashRejected':True,'modifiedMeasurementRejected':True,'publicKeyMatches':True,'privateKeyRead':False,'hardwareAttestation':False,'productionAdmission':False}
(h/'independent-signature-verification.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result))
