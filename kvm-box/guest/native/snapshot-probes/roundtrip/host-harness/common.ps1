$ErrorActionPreference='Stop'
$Run='snap-161ca971'; $Name='enclave-snap-161ca971'; $Dir='C:\Users\claude\enclave-snap-161ca971'; $Marker='private-snapshot-availability/snap-161ca971'
function Assert-Floor([long]$reserve) {
 $os=Get-CimInstance Win32_OperatingSystem
 if(([long]$os.FreePhysicalMemory*1024)-$reserve -lt 16GB){throw 'Remote host memory floor'}
 if((Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='C:'").FreeSpace -lt 48GB){throw 'Remote disk floor'}
}
function Owned-VM {
 $inventory=@(Get-VM -ErrorAction Stop)
 $record=Join-Path $Dir 'owned-id.txt'
 $ownedId=if(Test-Path -LiteralPath $record){(Get-Content -LiteralPath $record -Raw).Trim()}else{$null}
 $found=@($inventory | Where-Object {if($ownedId){$_.Id.Guid -eq $ownedId}else{$_.Name -eq $Name}})
 if($found.Count -gt 1){throw 'Ambiguous diagnostic VM'}
 if(-not $found.Count){return $null}
 $v=$found[0]
 $before=Get-Content -LiteralPath (Join-Path $Dir 'before.json') -Raw | ConvertFrom-Json
 if(@($before | Where-Object { $_.Id -eq $v.Id.Guid }).Count){throw 'Existing VM ID cannot be owned by probe'}
 $cs=Get-CimInstance -Namespace root\virtualization\v2 -Query ("select * from Msvm_ComputerSystem where Name = '"+$v.Id.Guid+"'")
 $s=$cs | Get-CimAssociatedInstance -ResultClass Msvm_VirtualSystemSettingData -Association Msvm_SettingsDefineState
 $gs=[IO.Path]::GetFullPath((Join-Path ([string]$s.GuestStateDataRoot) ([string]$s.GuestStateFile)))
 if([string]$s.FirmwareFile -ne (Join-Path $Dir 'probe.bin') -or $gs -ne (Join-Path $Dir 'probe.vmgs')){throw 'Diagnostic ownership paths mismatch'}
 if($v.Notes -and $v.Notes -ne $Marker){throw 'Diagnostic ownership marker mismatch'}
 if(-not $ownedId){$v.Id.Guid | Set-Content -LiteralPath $record}
 return $v
}
function Remove-Owned {
 $v=Owned-VM
 $id=$null
 if($v){$id=$v.Id.Guid; if([string]$v.State -ne 'Off'){Stop-VM -VM $v -TurnOff -Force -ErrorAction Stop}; Remove-VM -VM $v -Force -ErrorAction Stop
  if(@(Get-VM -ErrorAction Stop | Where-Object {$_.Id.Guid -eq $id}).Count){throw 'Diagnostic VM remains registered'}
 }
 if(@(Get-VM -ErrorAction Stop | Where-Object {$_.Name -eq $Name}).Count){throw 'Diagnostic name remains registered'}
 return @{id=$id;absent=$true;at=[DateTime]::UtcNow.ToString('o')}
}
