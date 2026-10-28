"""Synthetic two-cold-boot evidence. This never qualifies native app snapshots."""
import json,re
from pathlib import Path

def sequence(text, patterns):
    lines=text.splitlines();position=-1;matches=[]
    if 'ROUNDTRIP RESULT FAIL' in text: raise ValueError('guest failure')
    for pattern in patterns:
        found=[(i,m) for i,s in enumerate(lines) if (m:=re.fullmatch(pattern,s))]
        if len(found)!=1 or found[0][0]<=position: raise ValueError('missing/duplicate/out-of-order result')
        position,match=found[0];matches.append(match)
    return matches

def accept(report,before,after,cleanup):
    r=report.get('result.json',{});w=report.get('watchdog-receipt.json',{})
    if report.get('watchdog-error.json') or r.get('error') or r.get('cleanupError'):raise ValueError('lifecycle failure')
    if not all([r.get('cleanup',{}).get('absent'),w.get('absent'),w.get('secondAbsenceCheck',{}).get('absent'),cleanup.get('vmAbsent'),cleanup.get('tasksAbsent')]):raise ValueError('cleanup incomplete')
    identity=r.get('id','')
    if not re.fullmatch('[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}',identity):raise ValueError('owned identity missing')
    a={v['Id'].lower():v['State'] for v in before['vms']};b={v['Id'].lower():v['State'] for v in after['vms']}
    if a!=b or identity in a or identity in b or r['cleanup'].get('id')!=identity:raise ValueError('inventory/identity mismatch')
    a={v['path']:v['sha256'] for v in before['inputs']};b={v['path']:v['sha256'] for v in after['inputs']}
    master=next((h for p,h in a.items() if p.endswith('\\type1.vmgs')),None)
    if not master or a!=b or r.get('masterSha')!=master:raise ValueError('input/master pins changed')
    phases=r.get('phases',[])
    if len(phases)!=2 or [p['phase'] for p in phases]!=['capture','restore']:raise ValueError('two boots missing')
    if any(p.get('error') or not p.get('off') or not p.get('marker') for p in phases):raise ValueError('phase did not complete with actual power-off')
    if not phases[0]['finishedAt']<phases[1]['startedAt']:raise ValueError('boots overlap or timestamps missing')
    first=sequence(phases[0]['capture'],[r'ROUNDTRIP BEGIN encrypted-fresh-boot-v1 PUBLIC_TEST_KEY',r'ROUNDTRIP TPM_BEFORE OK',r'ROUNDTRIP CREATE begin',r'ROUNDTRIP CREATE OK original=1',r'ROUNDTRIP IMAGE_SIZE ([0-9]+)',r'ROUNDTRIP ENCRYPTED_SAVED bytes=([0-9]+)',r'ROUNDTRIP POST_CAPTURE_RAM_CHANGED',r'ROUNDTRIP CAPTURE_POWER_OFF'])
    second=sequence(phases[1]['capture'],[r'ROUNDTRIP BEGIN encrypted-fresh-boot-v1 PUBLIC_TEST_KEY',r'ROUNDTRIP AUTHENTICATED memory=([0-9]+) disk=([0-9]+)',r'ROUNDTRIP ATOMIC_RESTORE begin',r'ROUNDTRIP CREATE OK original=0',r'ROUNDTRIP RESUMED_CLOSE OK',r'ROUNDTRIP RAM_CONTINUATION OK bytes=1048576',r'ROUNDTRIP TPM_AFTER OK',r'ROUNDTRIP RESULT PASS synthetic-resume-only'])
    image,encrypted=int(first[4][1]),int(first[5][1]);memory,disk=int(second[1][1]),int(second[1][2])
    if not 0<image<=512*1024**2-4096 or image!=memory or disk!=len(b'synthetic-native-checkpoint-disk-state-v1'):raise ValueError('image/component length mismatch')
    payload=176+image+disk
    expected=40+payload+20*((payload+1048575)//1048576+1)
    if encrypted!=expected or encrypted>513*1024**2:raise ValueError('ciphertext frame size mismatch')
    if not re.fullmatch('[0-9a-f]{64}',r.get('captureVhdSha','')) or not 0<r.get('captureVhdBytes',0)<=1100*1024**2:raise ValueError('offline VHD receipt missing')
    return {'syntheticEncryptedColdBootContinuation':True,'memoryBytes':image,'tpmTransportAfterRestore':True,'desktopQualified':False,'writableFilesystemQualified':False,'freshAttestationQualified':False,'privateSnapshotAppQualified':False,'publicTestKey':True}

if __name__=='__main__':
    p=Path(__file__).resolve().parent
    print(json.dumps(accept(*[json.loads((p/f).read_text()) for f in ['remote-result.json','preflight-live.stdout','postflight-live.stdout','cleanup-verification.json']]),indent=2))
