# The Windows installer's smoke test (#14): installs the MSI for the current
# user with no UI, checks what it put where, runs the installed exe, removes
# it again and checks that nothing is left but the user's own settings.
# build-msi.ps1 builds the MSI; .github/workflows/release.yml runs both.
#
# Usage: packaging\windows\test-msi.ps1 -Msi <path> [-Log <path>]
# The installation log goes to -Log (default: beside the MSI); on a failure
# its last lines are printed.
param(
    [Parameter(Mandatory)] [string]$Msi,
    [string]$Log
)
$ErrorActionPreference = 'Stop'
$Msi = (Resolve-Path $Msi).Path
if (-not $Log) { $Log = [IO.Path]::ChangeExtension($Msi, '.install.log') }

$dir = "$env:LOCALAPPDATA\Programs\protoHDMIcapture"
$shortcut = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\protoHDMIcapture.lnk"
$key = 'HKCU:\Software\protoHDMIcapture'

function Fail([string]$why) {
    if (Test-Path $Log) {
        Write-Host '--- the end of the installation log:'
        Get-Content $Log -Tail 40 | Write-Host
    }
    throw "test-msi: $why"
}

function Msiexec([string]$what, [string]$arguments) {
    $p = Start-Process msiexec.exe -ArgumentList $arguments -Wait -PassThru
    if ($p.ExitCode -ne 0) { Fail "$what failed with exit code $($p.ExitCode)" }
}

if (Test-Path $dir) { Fail "$dir exists before the install: remove the installed viewer first" }

Msiexec 'the install' "/i `"$Msi`" /qn /l*v `"$Log`""
foreach ($f in 'protohdmicapture.exe', 'LICENSE-MIT.txt', 'LICENSE-APACHE.txt') {
    if (-not (Test-Path "$dir\$f")) { Fail "no $f in $dir" }
}
if (-not (Test-Path $shortcut)) { Fail "no Start menu shortcut at $shortcut" }
$target = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut).TargetPath
if ($target -ne "$dir\protohdmicapture.exe") { Fail "the shortcut points at $target" }
if (-not (Test-Path $key)) { Fail "no $key (the shortcut's key path)" }
# Per-user: no elevation, and the product registered for this user.
if (-not (Select-String -Path $Log -Pattern 'MSIINSTALLPERUSER = 1' -SimpleMatch -Quiet)) {
    Write-Host 'note: the log does not show MSIINSTALLPERUSER = 1'
}
$help = & "$dir\protohdmicapture.exe" --help
if ($LASTEXITCODE -ne 0 -or -not ($help -match '^usage: protohdmicapture')) { Fail "the installed exe's --help: exit $LASTEXITCODE" }

Msiexec 'the uninstall' "/x `"$Msi`" /qn /l*v+ `"$Log`""
foreach ($left in $dir, $shortcut, $key) {
    if (Test-Path $left) { Fail "$left is left after the uninstall" }
}
Write-Host "test-msi: installed, checked and removed $([IO.Path]::GetFileName($Msi))"
