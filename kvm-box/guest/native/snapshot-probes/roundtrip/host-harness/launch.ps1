. (Join-Path $PSScriptRoot 'common.ps1')
Assert-Floor 4GB
if((Get-Item -LiteralPath $Dir -ErrorAction Stop).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Diagnostic directory is a reparse point'}
$pins=Get-Content -LiteralPath (Join-Path $Dir 'stage-pins.json') -Raw | ConvertFrom-Json
foreach($f in $pins.PSObject.Properties){
 $item=Get-Item -LiteralPath (Join-Path $Dir $f.Name) -ErrorAction Stop
 if($item.Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Staged file is a reparse point'}
 if((Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash.ToLower() -ne $f.Value){throw 'Staged hash mismatch'}
}
$before=@(Get-VM -ErrorAction Stop | Select-Object Name,Id,@{N='State';E={[string]$_.State}})
if(@($before | Where-Object {$_.Name -eq $Name}).Count){throw 'VM name already present'}
$tasks=@(Get-ScheduledTask -ErrorAction Stop)
if(@($tasks | Where-Object {$_.TaskName -eq $Name -or $_.TaskName -eq ($Name+'-runner')}).Count){throw 'Task name already present'}
if(Test-Path -LiteralPath (Join-Path $Dir 'before.json')){throw 'Run already attempted'}
$before | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $Dir 'before.json')
$action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument ('-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "'+(Join-Path $Dir 'watchdog.ps1')+'"')
$trigger=New-ScheduledTaskTrigger -Once -At (Get-Date).AddSeconds(150) -RepetitionInterval (New-TimeSpan -Minutes 1)
$settings=New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds 45) -StartWhenAvailable -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $Name -Action $action -Trigger $trigger -Settings $settings -User 'SYSTEM' -RunLevel Highest -ErrorAction Stop | Out-Null
$action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument ('-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "'+(Join-Path $Dir 'run.ps1')+'"')
$trigger=New-ScheduledTaskTrigger -Once -At (Get-Date).AddSeconds(3)
$settings=New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds 120) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName ($Name+'-runner') -Action $action -Trigger $trigger -Settings $settings -User 'SYSTEM' -RunLevel Highest -ErrorAction Stop | Out-Null
@{registered=$true;run=$Run;watchdogAtSeconds=150;runnerLimitSeconds=120} | ConvertTo-Json -Compress
