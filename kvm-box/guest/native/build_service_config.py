#!/usr/bin/env python3
"""Stage the native profile's fixed service policy from the legacy image.

This preserves model/public-weight approvals and hashes; it does not authorize
models, authenticate storage, reserve a worker or admit the native profile.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat

DIGEST = re.compile(r'[0-9a-f]{64}\Z')
MODELS = {'llama':('qwen','qwen2.5-0.5b-instruct-q8_0.gguf'),
          'sd':('sd15','sd15-q8.gguf')}


def require(ok, reason):
    if not ok: raise ValueError(reason)


def read(path, limit):
    fd = os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
    with os.fdopen(fd,'rb') as stream:
        s = os.fstat(stream.fileno())
        require(stat.S_ISREG(s.st_mode) and s.st_size <= limit,'input type/size')
        data = stream.read(limit+1)
    require(len(data) <= limit,'input grew beyond limit')
    return data


def unique(pairs):
    out = {}
    for key, value in pairs:
        require(key not in out,'duplicate policy field')
        out[key] = value
    return out


def decode(data):
    return json.loads(data,object_pairs_hook=unique,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError('nonfinite policy value')))


def prepare(source, policies):
    raw = read(source,65536); config = decode(raw)
    require(isinstance(config,dict),'config object')
    require(set(config)=={'schema','sdk','core','socket','jobs','models','worker',
                         'threads','timeoutSeconds','clientUids','publicConstants','enableQ8Experimental'},'config fields')
    for key, expected in {'schema':'shield-guest-service/1','sdk':'/opt/shield',
                          'core':'/opt/shield/lib/libshield-core.so','socket':'/run/shield/compute.sock',
                          'jobs':'/run/shield/jobs','worker':{'host':'10.0.2.2','port':9741},
                          'clientUids':[0,1000]}.items():
        require(config[key]==expected,'unexpected legacy '+key)
    require(type(config['threads']) is int and 1 <= config['threads'] <= 16,'thread bound')
    require(type(config['timeoutSeconds']) is int and 1 <= config['timeoutSeconds'] <= 1200,'work deadline')
    require(type(config['enableQ8Experimental']) is bool,'Q8 policy type')
    constants=config['publicConstants']
    require(isinstance(constants,list) and len(constants)<=256 and
            all(isinstance(x,str) and DIGEST.fullmatch(x) for x in constants),'public constant digests')
    require(isinstance(config['models'],dict) and set(config['models'])==set(MODELS),'model set')
    policies=Path(policies).resolve(strict=True)
    for engine,(stem,filename) in MODELS.items():
        spec=config['models'][engine]
        require(isinstance(spec,dict) and set(spec)=={'model','modelSha256','policy','policySha256','calibration','reserveBytes'},'model fields')
        for key,value in {'model':'/models/'+filename,'policy':'/shield/policy/'+stem+'.json',
                          'calibration':'/shield/policy/'+stem+'.calib'}.items():
            require(spec[key]==value,'fixed model paths')
        require(type(spec['reserveBytes']) is int and 1 <= spec['reserveBytes'] <= 2_000_000_000,'reservation bound')
        for key in ['modelSha256','policySha256']:
            require(isinstance(spec[key],str) and DIGEST.fullmatch(spec[key]),'model/policy digest format')
        policy_raw=read(policies/(stem+'.json'),1<<20); policy=decode(policy_raw)
        require(hashlib.sha256(policy_raw).hexdigest()==spec['policySha256'],'policy digest mismatch')
        require(policy['modelSha256']==spec['modelSha256'],'model policy mismatch')
        calibration=read(policies/(stem+'.calib'),32<<20)
        require(hashlib.sha256(calibration).hexdigest()==policy['calibrationSha256'],'calibration digest mismatch')
    config['worker']={'host':'unix:/run/shield-worker/gpu0','port':19595}
    config['clientUids']=[1000,1003]
    data=(json.dumps(config,indent=2)+'\n').encode()
    receipt={'schema':'native-service-config-build/1','inputSha256':hashlib.sha256(raw).hexdigest(),
             'configSha256':hashlib.sha256(data).hexdigest(),'workerPeerUid':0,
             'changedFields':['worker','clientUids'],'modelApprovalsChanged':False,
             'authenticatedStorageEstablished':False,'workerReservationEstablished':False,
             'runtimeAdmission':False}
    return data,receipt


def build(source,policies,output):
    data,receipt=prepare(source,policies)
    output=Path(output); output.mkdir(mode=0o755)
    for name,body in [('native-service.json',data),('config-receipt.json',(json.dumps(receipt,indent=2)+'\n').encode())]:
        p=output/name
        with p.open('xb') as f: f.write(body)
        p.chmod(0o444)
    return receipt


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--source-config',required=True,type=Path)
    p.add_argument('--policies',required=True,type=Path)
    p.add_argument('--output',required=True,type=Path)
    a=p.parse_args()
    print(json.dumps(build(a.source_config,a.policies,a.output)))
