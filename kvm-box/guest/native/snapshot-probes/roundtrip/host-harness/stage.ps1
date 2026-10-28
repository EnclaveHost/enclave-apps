$ErrorActionPreference='Stop'
$Dir='C:\Users\claude\enclave-snap-161ca971'
if(Test-Path -LiteralPath $Dir){throw 'Exclusive diagnostic directory already exists'}
$parent=Get-Item -LiteralPath (Split-Path -Parent $Dir) -ErrorAction Stop
if($parent.Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Diagnostic parent is a reparse point'}
New-Item -ItemType Directory -Path $Dir -ErrorAction Stop | Out-Null
if((Get-Item -LiteralPath $Dir).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Diagnostic directory is a reparse point'}
@{created=$Dir} | ConvertTo-Json -Compress
