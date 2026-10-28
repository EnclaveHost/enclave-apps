. (Join-Path $PSScriptRoot 'common.ps1')
try {
 # The creator and boot loop run in one dedicated task with a 120-second
 # host-enforced lifetime. Quiesce that exact task before examining VM state.
 $taskName=$Name+'-runner'
 $task=Get-ScheduledTask -TaskName $taskName -ErrorAction Stop
 Stop-ScheduledTask -TaskName $taskName -ErrorAction Stop
 $deadline=(Get-Date).AddSeconds(10)
 while((Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).State -eq 'Running'){
  if((Get-Date) -gt $deadline){throw 'Owned runner did not stop'}
  Start-Sleep -Milliseconds 200
 }
 $record=Join-Path $Dir 'runner-process.json'
 if(Test-Path -LiteralPath $record){
  $r=Get-Content -LiteralPath $record -Raw | ConvertFrom-Json
  $process=@(Get-Process -ErrorAction Stop | Where-Object {$_.Id -eq $r.pid})
  if($process.Count -and $process[0].StartTime.ToUniversalTime().Ticks -eq $r.startTicks){throw 'Owned creator process is still alive'}
 }
 $receipt=Remove-Owned
 # An interrupted DefineSystem may still be running inside VMMS. Do not
 # declare absence final or cancel this watchdog while that is unresolved.
 if((Test-Path -LiteralPath (Join-Path $Dir 'create-started.txt')) -and -not (Test-Path -LiteralPath (Join-Path $Dir 'create-returned.txt'))){throw 'Interrupted VM definition: watchdog retained for reconciliation'}
 Start-Sleep -Seconds 2
 $second=Remove-Owned
 $receipt.secondAbsenceCheck=$second
 $receipt | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $Dir 'watchdog-receipt.json')
 Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction Stop
 Unregister-ScheduledTask -TaskName $Name -Confirm:$false -ErrorAction Stop
} catch {
 @{error=[string]$_.Exception.Message;at=[DateTime]::UtcNow.ToString('o')} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Dir 'watchdog-error.json')
 exit 1
}
