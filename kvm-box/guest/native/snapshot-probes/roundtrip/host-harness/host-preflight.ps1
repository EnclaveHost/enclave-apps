$ErrorActionPreference = 'Stop'
if (-not (Get-Command Get-VM -ErrorAction SilentlyContinue)) { throw 'Host VM inventory unavailable' }
$os = Get-CimInstance Win32_OperatingSystem
$disk = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='C:'"
$files = @('C:\Program Files\Enclave\guest\hyperv.psm1','C:\Program Files\Enclave\guest\utilities.psm1','C:\ProgramData\Enclave\shield\type1.vmgs')
$pins = @($files | ForEach-Object { if (-not (Test-Path -LiteralPath $_ -PathType Leaf)) { throw ('Required input missing: ' + $_) }; @{path=$_;sha256=(Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash.ToLower();bytes=(Get-Item -LiteralPath $_).Length} })
$registry = Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization' -Name AllowFirmwareLoadFromFile
@{checkedAt=[DateTime]::UtcNow.ToString('o');host=$env:COMPUTERNAME;availableMemoryBytes=[int64]$os.FreePhysicalMemory*1024;totalMemoryBytes=[int64]$os.TotalVisibleMemorySize*1024;rootFreeBytes=[int64]$disk.FreeSpace;firmwareFileOptIn=[int]$registry.AllowFirmwareLoadFromFile;inputs=$pins;vms=@(Get-VM | Select-Object Name,Id,@{N='State';E={[string]$_.State}},MemoryAssigned,ProcessorCount)} | ConvertTo-Json -Depth 5 -Compress
