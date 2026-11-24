. (Join-Path $PSScriptRoot 'common.ps1')
$self=Get-Process -Id $PID
@{pid=$PID;startTicks=$self.StartTime.ToUniversalTime().Ticks}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $Dir 'runner-process.json')
$report=@{run=$Run;startedAt=[DateTime]::UtcNow.ToString('o');phases=@()}
function Boot-Phase([string]$phase,[string]$marker) {
 Assert-Floor 4GB
 $v=Owned-VM; if(-not $v -or [string]$v.State -ne 'Off'){throw 'Owned VM is not Off before cold boot'}
 $drives=@(Get-VMHardDiskDrive -VM $v -ErrorAction Stop)
 if($drives.Count -ne 1 -or $drives[0].Path -ne (Join-Path $Dir 'synthetic.vhdx')){throw 'Diagnostic disk attachment mismatch'}
 if(@(Get-VMNetworkAdapter -VM $v -ErrorAction Stop).Count){throw 'Unexpected NIC'}
 $phaseReport=@{phase=$phase;startedAt=[DateTime]::UtcNow.ToString('o');capture='';off=$false;marker=$false}
 $pipe=$null
 try {
  $pipe=New-Object IO.Pipes.NamedPipeClientStream('.', $Run, [IO.Pipes.PipeDirection]::In, [IO.Pipes.PipeOptions]::Asynchronous)
  $connection=$pipe.ConnectAsync(10000)
  $deadline=(Get-Date).AddSeconds(45)
  Start-VM -VM $v -ErrorAction Stop
  if(-not $connection.Wait(10000) -or -not $pipe.IsConnected){throw 'Diagnostic console did not connect'}
  $buf=New-Object byte[] 4096; $pending=$null; $eof=$false
  while((Get-Date) -lt $deadline){
   Assert-Floor 0
   if((Get-Item -LiteralPath (Join-Path $Dir 'synthetic.vhdx') -ErrorAction Stop).Length -gt 1100MB){throw 'Diagnostic disk backing exceeds bound'}
   if(-not $eof){
    try {
     if($null -eq $pending){$pending=$pipe.ReadAsync($buf,0,$buf.Length)}
     if($pending.Wait(250)){
      $n=$pending.Result;$pending=$null
      if($n -gt 0){$phaseReport.capture += [Text.Encoding]::ASCII.GetString($buf,0,$n)}else{$eof=$true}
      if($phaseReport.capture.Length -gt 262144){throw 'Phase serial capture exceeds bound'}
     }
    } catch [System.IO.IOException] {$eof=$true}
   }else{Start-Sleep -Milliseconds 100}
   if($phaseReport.capture.Contains('ROUNDTRIP RESULT FAIL')){throw 'Guest reported failure'}
   if($phaseReport.capture.Contains($marker)){$phaseReport.marker=$true}
   $state=Owned-VM
   if(-not $state){throw 'Owned VM disappeared during boot'}
   if([string]$state.State -eq 'Off'){$phaseReport.off=$true;break}
  }
  if(-not $phaseReport.marker -or -not $phaseReport.off){throw 'Required marker and actual power-off not observed in 45s'}
 } catch {$phaseReport.error=[string]$_.Exception.Message}
 finally {if($pipe){$pipe.Dispose()};$phaseReport.finishedAt=[DateTime]::UtcNow.ToString('o')}
 return $phaseReport
}
try {
 Assert-Floor 4GB
 $definition=& (Join-Path $Dir 'create.ps1')
 $definition|Set-Content -LiteralPath (Join-Path $Dir 'definition.json')
 $v=Owned-VM;if(-not $v){throw 'No owned diagnostic VM'};$report.id=$v.Id.Guid
 if(@(Get-VMHardDiskDrive -VM $v -ErrorAction Stop).Count){throw 'Preexisting disk in new diagnostic VM'}
 $disk=Join-Path $Dir 'synthetic.vhdx'
 if(Test-Path -LiteralPath $disk){throw 'Diagnostic VHD already exists'}
 New-VHD -Path $disk -SizeBytes 1GB -Dynamic -ErrorAction Stop|Out-Null
 $vh=Get-VHD -Path $disk -ErrorAction Stop
 if($vh.Size -ne 1GB -or [string]$vh.VhdType -ne 'Dynamic' -or $vh.Attached){throw 'New VHD verification failed'}
 if((Get-Item -LiteralPath $disk).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Diagnostic VHD is a reparse point'}
 $controllers=@(Get-VMScsiController -VM $v -ErrorAction Stop)
 if(-not $controllers.Count){Add-VMScsiController -VM $v -ErrorAction Stop|Out-Null}
 Add-VMHardDiskDrive -VM $v -ControllerType SCSI -ControllerNumber 0 -ControllerLocation 0 -Path $disk -ErrorAction Stop
 $report.diskPath=$disk
 $first=Boot-Phase 'capture' 'ROUNDTRIP CAPTURE_POWER_OFF';$report.phases+=,$first
 if($first.error){throw 'Capture phase failed; restore prohibited'}
 $lines=$first.capture -split '\r?\n'
 if(@($lines|Where-Object {$_ -eq 'ROUNDTRIP CREATE OK original=1'}).Count -ne 1){throw 'Original capture branch not confirmed'}
 if(@($lines|Where-Object {$_ -match '^ROUNDTRIP ENCRYPTED_SAVED bytes=[0-9]+$'}).Count -ne 1){throw 'Encrypted frame not confirmed'}
 if(@($lines|Where-Object {$_ -eq 'ROUNDTRIP POST_CAPTURE_RAM_CHANGED'}).Count -ne 1){throw 'Sentinel mutation not confirmed'}
 $report.captureVhdSha=(Get-FileHash -LiteralPath $disk -Algorithm SHA256).Hash.ToLower()
 $report.captureVhdBytes=(Get-Item -LiteralPath $disk).Length
 # Full power-off was observed; recheck floors and ownership before second boot.
 Assert-Floor 4GB
 $second=Boot-Phase 'restore' 'ROUNDTRIP RESULT PASS synthetic-resume-only';$report.phases+=,$second
 if($second.error){throw 'Restore phase failed'}
} catch {$report.error=[string]$_.Exception.Message}
finally {
 try {$report.cleanup=Remove-Owned}catch{$report.cleanupError=[string]$_.Exception.Message}
 $report.finishedAt=[DateTime]::UtcNow.ToString('o')
 $report.after=@(Get-VM -ErrorAction Stop|Select-Object Name,Id,@{N='State';E={[string]$_.State}})
 $report.masterSha=(Get-FileHash -LiteralPath 'C:\ProgramData\Enclave\shield\type1.vmgs' -Algorithm SHA256).Hash.ToLower()
 $report|ConvertTo-Json -Depth 8|Set-Content -LiteralPath (Join-Path $Dir 'result.json')
 $report|ConvertTo-Json -Depth 8 -Compress
}
'done'|Set-Content -LiteralPath (Join-Path $Dir 'runner-complete.txt')
if($report.error -or $report.cleanupError){exit 1}
