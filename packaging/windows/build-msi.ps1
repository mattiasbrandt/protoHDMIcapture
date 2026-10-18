# The Windows installer (#14): builds the viewer in release mode with the
# locked dependencies (the C runtime linked in: viewer/.cargo/config.toml),
# then packaging/windows/protohdmicapture.wxs with WiX 7 into
# OUT_DIR\protohdmicapture-<version>-x86_64-windows.msi, validates it (the
# ICE checks), and prints its path.
#
# Usage: packaging\windows\build-msi.ps1 [-OutDir dist]
# Needs: the Rust toolchain for x86_64-pc-windows-msvc and the WiX 7 .NET
# tool (dotnet tool install --global wix --version 7.0.0).
#
# WiX 7 comes under the Open Source Maintenance Fee EULA
# (https://docs.firegiant.com/wix/osmf/): free for this project, which earns
# nothing; organizations earning over US$10,000 a year from projects that
# use WiX sponsor it. Accepted for this project by its owner on 2026-10-19
# (#14), hence -acceptEula wix7 below. WiX 3 to 5 left support in 2026.
param([string]$OutDir = "dist")
$ErrorActionPreference = 'Stop'

# A native command's exit code, said if not 0. Its stderr (cargo's progress)
# is no error: Windows PowerShell 5.1 would stop on it under 'Stop'.
function Invoke-Native([string]$what, [scriptblock]$command) {
    $ErrorActionPreference = 'Continue'
    & $command
    if ($LASTEXITCODE -ne 0) { throw "build-msi: $what failed (exit $LASTEXITCODE)" }
}

$root = (Resolve-Path "$PSScriptRoot\..\..").Path
$version = (Select-String -Path "$root\viewer\Cargo.toml" -Pattern '^version = "(.*)"$' | Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw "build-msi: no version in viewer\Cargo.toml" }

Invoke-Native 'cargo build' { cargo build --manifest-path "$root\viewer\Cargo.toml" --release --locked }
$target = (cargo metadata --manifest-path "$root\viewer\Cargo.toml" --format-version 1 --no-deps | ConvertFrom-Json).target_directory

$wixVersion = (wix --version) -replace '\+.*$', ''
if ($wixVersion -notlike '7.*') { throw "build-msi: WiX 7 is needed, found $wixVersion" }

New-Item -ItemType Directory -Force $OutDir | Out-Null
$msi = Join-Path (Resolve-Path $OutDir).Path "protohdmicapture-$version-x86_64-windows.msi"
Invoke-Native 'wix build' {
    wix build "$PSScriptRoot\protohdmicapture.wxs" -arch x64 -acceptEula wix7 `
        -d "Version=$version" -d "Exe=$target\release\protohdmicapture.exe" `
        -d "Icon=$PSScriptRoot\protohdmicapture.ico" -d "Root=$root" `
        -o $msi
}
Invoke-Native 'wix msi validate' { wix msi validate $msi -acceptEula wix7 }
# wix writes the debug symbols beside the msi; the release carries the msi only.
Remove-Item ([IO.Path]::ChangeExtension($msi, '.wixpdb')) -ErrorAction SilentlyContinue
$msi
