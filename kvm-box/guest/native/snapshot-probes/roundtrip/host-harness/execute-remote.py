"""Owned diagnostic staging/launch. No invocation before coordinated host slot."""
from pathlib import Path
import base64,json,subprocess,hashlib,sys,time,shutil
p=Path(__file__).resolve().parent
plan=json.loads((p/'remote-plan.json').read_text())
for f,h in plan['files'].items():
 assert hashlib.sha256((p/f).read_bytes()).hexdigest()==h
assert shutil.disk_usage('/').free >= 48*1024**3
ssh=['ssh','-o','BatchMode=yes','-o','StrictHostKeyChecking=yes','-o','ConnectTimeout=6','minipc']
def ps(source,stem,timeout=25):
 encoded=base64.b64encode(source.encode('utf-16le')).decode()
 r=subprocess.run(ssh+['powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand '+encoded],capture_output=True,text=True,timeout=timeout)
 (p/(stem+'.stdout')).write_text(r.stdout);(p/(stem+'.stderr')).write_text(r.stderr)
 if r.returncode:raise RuntimeError(stem+' failed; see retained receipt')
 return r.stdout
if sys.argv[1:] != ['--stage-and-launch']:raise SystemExit('Explicit --stage-and-launch required')
ps((p/'host-preflight.ps1').read_text(),'preflight-live')
pre=json.loads((p/'preflight-live.stdout').read_text());assert pre['availableMemoryBytes']>=20*1024**3 and pre['rootFreeBytes']>=48*1024**3
ps("$ErrorActionPreference='Stop';foreach($s in @('MachinePolicy','UserPolicy')){if((Get-ExecutionPolicy -Scope $s) -ne 'Undefined'){throw 'Centrally managed policy'}};@{ordinaryDefault=$true}|ConvertTo-Json -Compress",'policy-live')
ps((p/'stage.ps1').read_text(),'stage')
files=[p/k for k in plan['files'] if k!='stage.ps1']+[p/'stage-pins.json']
for f in files:
 r=subprocess.run(['scp','-q','-o','BatchMode=yes','-o','StrictHostKeyChecking=yes','-o','ConnectTimeout=6',str(f),'minipc:'+plan['directory'].replace('\\','/')+'/'+f.name],capture_output=True,text=True,timeout=25)
 if r.returncode:raise RuntimeError('script copy failed')
image=Path('/home/steven/Documents/Codex/2026-10-27/re/work/native-snapshot-roundtrip-build2/igvm/vbs-linux-cd5b06de/vbs-linux-candidate.bin')
assert hashlib.sha256(image.read_bytes()).hexdigest()=='cd5b06ded55dec460547531e47ca4de22775b9fc11e1d52d58c9e34e92df41e7'
r=subprocess.run(['scp','-q','-l','32768','-o','BatchMode=yes','-o','StrictHostKeyChecking=yes','-o','ConnectTimeout=6',str(image),'minipc:'+plan['directory'].replace('\\','/')+'/probe.bin'],capture_output=True,text=True,timeout=45)
if r.returncode:raise RuntimeError('image copy failed')
# Parse every script before registering any task. Verify the pins file itself
# using this locally pinned expected digest, then launch performs file checks.
pins=hashlib.sha256((p/'stage-pins.json').read_bytes()).hexdigest()
d=plan['directory']
source=f"""$ErrorActionPreference='Stop';$dir='{d}';
if((Get-FileHash -LiteralPath ($dir+'\\stage-pins.json') -Algorithm SHA256).Hash.ToLower() -ne '{pins}'){{throw 'Pin file changed'}};
foreach($f in Get-ChildItem -LiteralPath $dir -Filter '*.ps1'){{$t=$null;$e=$null;[System.Management.Automation.Language.Parser]::ParseFile($f.FullName,[ref]$t,[ref]$e)|Out-Null;if($e.Count){{throw ('Parse failed: '+$f.Name)}}}};
& ($dir+'\\launch.ps1')
"""
print(ps(source,'launch'),flush=True)
# The independent host watchdog is already active. This process does not need
# to stay alive to turn off/remove the owned VM. Collect without long blocking.
for i in range(38):
 time.sleep(5)
 source=f"$d='{d}';$out=@{{}};foreach($n in @('result.json','watchdog-receipt.json','watchdog-error.json')){{if(Test-Path -LiteralPath ($d+'\\'+$n)){{$out[$n]=Get-Content -LiteralPath ($d+'\\'+$n) -Raw | ConvertFrom-Json}}}};$out|ConvertTo-Json -Depth 8 -Compress"
 out=json.loads(ps(source,'poll',15))
 if out.get('watchdog-receipt.json'):
  (p/'remote-result.json').write_text(json.dumps(out,indent=2)+'\n');print(json.dumps(out));break
else:raise RuntimeError('Watchdog completion unconfirmed; retain task and reconcile, do not rerun')
ps((p/'host-preflight.ps1').read_text(),'postflight-live')
