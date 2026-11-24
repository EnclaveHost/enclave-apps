# The NucBox side of svn26.sh. Run with $mode set by the caller (svn26.sh prepends it); every run ends with a line
# starting DONE or STOP. Modes: status, prepare, verify, apply, revert, launch. Made after
# native-shield-image-v22/activation/svn25-box.ps1. New this time: the model disk (models.iso) is staged too, and
# prepare moves it in from $rel\..\staging-svn26 when it was uploaded there ahead of the release folder.
#
# Where this box keeps the native desktop release since host release 2026.11.18
# (docs/operations/windows-release-2026.11.18.md): the two local hooks, which an install never writes, each call the
# release folder's native-desktop-env.cmd, and the node's hook names the box's own admitted-image policy. The Shield
# manager reads the image when it starts, so the switch is: hooks and policy, then the installed bundle installed
# again (the way every release since 2026.11.18 has restarted the roles; recovery after it is hands-off).
$ErrorActionPreference = 'Stop'
$root   = 'C:\ProgramData\Enclave'
$rel    = "$root\native-desktop\releases"
$old    = '8695919cb134018d3ce58e771ad9053a0586f543ac8ccea3bc2bbfbe41f0840e'
$new    = '07c4efd2fe3db9bc906e0d89d4db62284ed5747d534d204efbd4e758cbbc24a1'
$hooks  = @("$root\node-config.local.cmd", "$root\shield-config.local.cmd")
$pol    = "$root\native-desktop\admitted-shield-policy.json"
$polOld = 'ae6326749c2b30b6317f41487bff932c448d560b3ba4e3e694a235a3226e31dc'
$polNew = '866bc49effcf7fb588af0f2b98feb4b064b3c7a8bf2afb4a8893294242d6bfe1'
$stamp  = 'before-native-svn26-20261124'
$W      = 'C:\Users\claude\bundle-build'
$bundle = @{ dir = "$W\out50"; version = '2026.11.20'; zip = '01c7255d9a2c88624155320ee5b9050738de6bb7c6604e10e169f782fbf240f7'; log = "$W\install50-svn26.log" }
$kvm    = '0xf9bb979a'
$staged = [ordered]@{
  'native-igvm.bin'        = '07c4efd2fe3db9bc906e0d89d4db62284ed5747d534d204efbd4e758cbbc24a1'
  'root.iso'               = '5446eebafd70cf8c285ac25a10f7ae357d7a041ea4eccd87cce67be4ea86b7c7'
  'hashes.iso'             = '0631870caac2c8cb81d65a7af862dde70e4d69376cc5f2a8e6c997e4d509e541'
  'models.iso'             = '2887bde950db7bca139b5b4317cc2027ac391e601068d6d75197d157924db964'
  'profile.json'           = 'c84c58bf0e5140d475f2f106a3ceddf5c7e090467b56b1b60d53cb66058a8b3a'
  'desktop-config.json'    = 'ac8aade4979e3d384c4acb7eda764a75d2e8cd82ae8d4c49be28f335d03f9e1c'
  'desktop-release.json'   = 'be76d7d744d607a2a90c6a013c5f1730fb38e1c6a7594ef4296a68f19c13048d'
  'native-desktop-env.cmd' = 'dd8a3cf4544e8f746a9e3a50dcebca5a385cdcfd7e9b38d2c3b999faf1b1b98c'
  'shield-policy.json'     = $polNew
  'shielded-bridge.exe'    = 'cc751de67c7d303f989367e1ac5ed5eb316f2b13cd10be74ff44256551d18d73'
}

function Sha($p) { (Get-FileHash -LiteralPath $p -Algorithm SHA256).Hash.ToLower() }
function Cut($s,$n) { $s=[string]$s; $s.Substring(0,[Math]::Min($n,$s.Length)) }
function Health { try { (Invoke-WebRequest -UseBasicParsing -Uri 'http://127.0.0.1:8091/health' -TimeoutSec 5).Content | ConvertFrom-Json } catch { $null } }
function HealthLine($h) { if ($h) { "canStart=$($h.canStart) desktop.ready=$($h.desktop.ready) igvm=$(Cut $h.desktop.igvmSha256 12)" } else { 'no answer' } }
function Names($f) {
  $t = Get-Content -LiteralPath $f -Raw
  $o = $t.Contains("releases\$old\native-desktop-env.cmd"); $n = $t.Contains("releases\$new\native-desktop-env.cmd")
  if ($o -and -not $n) { 'old' } elseif ($n -and -not $o) { 'new' } else { 'neither or both' }
}
function Installed { try { (Get-Content 'C:\Program Files\Enclave\manifest.json' -Raw | ConvertFrom-Json).version } catch { 'unknown' } }
function Keep($f) { $b = "$f.$stamp"; if (-not (Test-Path -LiteralPath $b)) { Copy-Item -LiteralPath $f -Destination $b } }
# Returns one boolean and nothing else: the problems go to the host stream, not into the answer. (svn24-box.ps1
# returned its MISMATCH lines with the boolean, and an array of a line and $false is true to an if.)
function Verify {
  $bad = 0
  foreach ($n in $staged.Keys) { $p = "$rel\$new\$n"; if (-not (Test-Path -LiteralPath $p)) { Write-Host "MISSING $n"; $bad++ } elseif ((Sha $p) -ne $staged[$n]) { Write-Host "MISMATCH $n"; $bad++ } }
  return ($bad -eq 0)
}
function NodeLines($since) {
  $day = (Get-Date).ToUniversalTime().ToString('HH:mm:ss')
  @(Get-Content "$root\logs\node.log" -Tail 4000 | Where-Object { $_ -match $kvm -and ($_.Substring(0,8) -ge $since -or $since -gt $day) } | Select-Object -Last 8) | ForEach-Object { '  ' + (Cut $_ 300) }
}
# A full install of the installed bundle again, as SYSTEM, the way 2026.11.18.1 and 2026.11.20 were installed.
function Install {
  $b = $bundle; $zip = "$($b.dir)\enclave-host-$($b.version).zip"
  $script:installed = $false
  if (-not (Test-Path -LiteralPath $zip) -or (Sha $zip) -ne $b.zip) { "$zip is not the built bundle; nothing installed"; return }
  $script = "$W\doinstall-$($b.version)-svn26.ps1"
  @"
`$log = '$($b.log)'
"start `$((Get-Date).ToUniversalTime().ToString('o'))" | Set-Content `$log
Get-VM | ForEach-Object { "vm-before `$(`$_.Name) `$(`$_.State)" } | Add-Content `$log
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File '$($b.dir)\work\enclave-host\install-host.ps1' -Name nucbox-k11-4 -Owner 'NUCBOX_K11\srbat' -SetupExe '$($b.dir)\EnclaveSetup.exe' -ManageWindowsUpdate off *>> `$log
"exit `$LASTEXITCODE `$((Get-Date).ToUniversalTime().ToString('o'))" | Add-Content `$log
"@ | Set-Content -LiteralPath $script
  $task = 'EnclaveInstallSvn26'
  $a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$script`""
  Register-ScheduledTask -TaskName $task -Action $a -Principal (New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest) -Force | Out-Null
  if (Test-Path -LiteralPath $b.log) { Remove-Item -LiteralPath $b.log }
  Start-ScheduledTask -TaskName $task
  "install of $($b.version) started as SYSTEM; its log is $($b.log)"
  $done = $null
  for ($i = 0; $i -lt 120 -and -not $done; $i++) {
    Start-Sleep -Seconds 10
    # The installer's lines are appended as UTF-16 and ours as ANSI, so the last line arrives with a NUL in front of
    # it: without the -replace this never matched, and the loop ran its twenty minutes after a finished install
    # (2026-11-20, and 2026-11-24 until it was stopped by hand before it put the hooks back).
    if (Test-Path -LiteralPath $b.log) { $done = @(Get-Content -LiteralPath $b.log -Tail 1) -replace "`0", '' | Where-Object { $_ -match '^exit ' } }
  }
  Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
  if (Test-Path -LiteralPath $b.log) { Get-Content -LiteralPath $b.log -Tail 12 | ForEach-Object { '  ' + (Cut $_ 220) } }
  if (-not $done) { 'the install did not finish within twenty minutes; its log is above'; return }
  if ($done -notmatch '^exit 0 ') { "the install ended with: $done"; return }
  if ((Installed) -ne $b.version) { "the install says it finished but the box reports $(Installed)"; return }
  $script:installed = $true
}
function Point($from,$to) {
  foreach ($f in $hooks) {
    $t = Get-Content -LiteralPath $f -Raw
    Set-Content -LiteralPath $f -Value ($t.Replace("releases\$from\native-desktop-env.cmd", "releases\$to\native-desktop-env.cmd")) -NoNewline
  }
}
# apply: hooks and policy to the new release, then the bundle again. revert: the same back. If the install fails, the
# hooks and the policy are put back as they were, so what the roles read still agrees with what was running.
function Switch-Box($which) {
  $from, $to, $pFrom, $pTo = if ($which -eq 'new') { $old, $new, $polOld, $polNew } else { $new, $old, $polNew, $polOld }
  $state = @($hooks | ForEach-Object { Names $_ }); $ps = Sha $pol
  $want = if ($which -eq 'new') { 'new' } else { 'old' }
  $h = Health
  if (($state | Where-Object { $_ -ne $want }).Count -eq 0 -and $ps -eq $pTo -and $h -and $h.desktop.igvmSha256 -eq $to) {
    "DONE box (already on this release: bundle $(Installed), manager $(HealthLine $h))"; return
  }
  $have = if ($which -eq 'new') { 'old' } else { 'new' }
  $half = (($state | Where-Object { $_ -ne $want }).Count -eq 0 -and $ps -eq $pTo)   # hooks and policy moved, manager not yet
  if (-not $half) {
    if (($state | Where-Object { $_ -ne $have }).Count -ne 0 -or $ps -ne $pFrom) { "STOP: the hooks name [$($state -join ', ')] and the policy is $(Cut $ps 16): not the state this step starts from; nothing changed"; return }
    if ((Installed) -ne $bundle.version) { "STOP: the box runs bundle $(Installed), not $($bundle.version); nothing changed"; return }
    if ($which -eq 'new' -and -not (Verify)) { 'STOP: the staged release folder does not verify; nothing changed'; return }
    $source = if ($which -eq 'new') { "$rel\$new\shield-policy.json" } else { "$pol.$stamp" }
    if (-not (Test-Path -LiteralPath $source) -or (Sha $source) -ne $pTo) { "STOP: $source is not the policy this step installs; nothing changed"; return }
    foreach ($f in $hooks + $pol) { Keep $f }
    Point $from $to; Copy-Item -LiteralPath $source -Destination $pol -Force
    "hooks now call releases\$(Cut $to 12)...; the box's policy is $(Cut (Sha $pol) 16)"
  } else { "hooks and policy already name releases\$(Cut $to 12)...; installing the bundle again so the roles read them" }
  Install
  if (-not $script:installed) {
    Point $to $from; Copy-Item -LiteralPath $(if ($which -eq 'new') { "$pol.$stamp" } else { "$rel\$new\shield-policy.json" }) -Destination $pol -Force
    "STOP: hooks and policy are back as they were (policy $(Cut (Sha $pol) 16)); the bundle reported is $(Installed). Roles the installer stopped may need '$($bundle.version)' installed again"
    return
  }
  $h = $null; for ($i = 0; $i -lt 60; $i++) { $h = Health; if ($h -and $h.desktop.igvmSha256 -eq $to) { break }; Start-Sleep -Seconds 5 }
  "manager: $(HealthLine $h)"
  if (-not $h -or $h.desktop.igvmSha256 -ne $to) { "STOP: bundle $(Installed) is installed and the hooks and policy name this release, but the manager does not report it" ; return }
  "DONE box (bundle $(Installed); previous hooks and policy kept as *.$stamp)"
}

switch ($mode) {
  'prepare' {
    $dir = "$rel\$new"
    if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null; "created $dir" }
    $b = "$dir\shielded-bridge.exe"
    if (-not (Test-Path -LiteralPath $b)) { Copy-Item -LiteralPath "$rel\$old\shielded-bridge.exe" -Destination $b; 'copied shielded-bridge.exe from the SVN25 release' }
    $up = "$root\native-desktop\staging-svn26\models.iso"; $m = "$dir\models.iso"
    if ((Test-Path -LiteralPath $up) -and -not (Test-Path -LiteralPath $m)) { Move-Item -LiteralPath $up -Destination $m; 'moved models.iso in from the staging folder' }
    if (Test-Path -LiteralPath $m) { "HAVE models.iso $((Get-Item -LiteralPath $m).Length)" }
    if ((Sha $b) -ne $staged['shielded-bridge.exe']) { 'STOP: the bridge in the release folder is not the required one' } else { "free on C: $([Math]::Round((Get-PSDrive C).Free/1GB,1)) GB"; 'DONE prepare' }
  }
  'verify' { if (Verify) { 'DONE staged' } else { 'STOP: staged files differ' } }
  'status' {
    "bundle: $(Installed)"
    foreach ($f in $hooks) { "$(Split-Path $f -Leaf) names: $(Names $f)" }
    $ps = Sha $pol; "box policy: $(if ($ps -eq $polNew) { 'SVN26' } elseif ($ps -eq $polOld) { 'SVN25 (previous)' } else { 'another file ' + (Cut $ps 16) })"
    "staged: $(if (Test-Path -LiteralPath "$rel\$new") { if (Verify) { 'yes' } else { 'no' } } else { 'no folder' })"
    "manager: $(HealthLine (Health))"
    Get-ScheduledTask -TaskPath '\Enclave\' | Where-Object { $_.TaskName -in 'Shield','Node','Network','Card','Relay' } | ForEach-Object { "task $($_.TaskName): $($_.State)" }
    $vm = @(Get-VM | Where-Object { $_.Name -match 'enclave-app' }); "app VMs: $($vm.Count) ($(@($vm | Where-Object { $_.State -eq 'Running' }).Count) running)"
    "node on ${kvm}:"; NodeLines '00:00:00'
    'DONE status'
  }
  'apply'  { Switch-Box 'new' }
  'revert' { Switch-Box 'old' }
  'launch' {
    $vm = @(Get-VM | Where-Object { $_.Name -match 'enclave-app' } | ForEach-Object { "$($_.Name) $($_.State) $([Math]::Round($_.MemoryAssigned/1GB,1))GB up=$($_.Uptime.ToString('hh\:mm\:ss'))" })
    "manager: $(HealthLine (Health)); app VMs: $($vm.Count)"
    $lines = NodeLines $since; $lines
    $text = $lines -join "`n"
    if ($text -match "$kvm isolation adopted.*image=$new|claimed $kvm") { 'DONE launch running' }
    elseif ($text -match 'not claimed|giving up|refused') { 'DONE launch refused' }
    else { 'DONE launch waiting' }
  }
  default { "STOP: no such mode: $mode" }
}
