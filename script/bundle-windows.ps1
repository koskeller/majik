# Build Majik and wrap it in an Inno Setup installer.
#
#   script/bundle-windows.ps1 [-Architecture x86_64]
#
# Code signing is not wired up yet: Majik has no Authenticode certificate, so the installer is
# unsigned and SmartScreen will warn on download. The shape here matches script/bundle-mac so adding
# a certificate later is a secrets change and nothing else.
[CmdletBinding()]
param(
    [Parameter()][Alias('a')][string]$Architecture = "x86_64"
)

$ErrorActionPreference = "Stop"

# Mirrors script/lib/release.sh. This file is the Windows half of the pair that
# `config::tests::every_bundle_script_greps_for_the_marker_the_binary_emits` pins.
$ReleaseChannel = "stable"
$ChannelMarkerPrefix = "majik-channel:"

$RepoRoot = (& git rev-parse --show-toplevel).Trim()
Set-Location $RepoRoot

if ($Architecture -ne "x86_64") {
    throw "Unsupported architecture '$Architecture'. Only x86_64 is built today; aarch64 Windows is untested."
}
$Target = "$Architecture-pc-windows-msvc"

$Version = (cargo metadata --no-deps --format-version=1 | ConvertFrom-Json).packages |
    Where-Object { $_.name -eq "majik-app" } | Select-Object -ExpandProperty version

Write-Host "Building Majik $Version ($ReleaseChannel) for $Target"

# Everything below this line is a stable-channel build.
$env:MAJIK_CHANNEL = $ReleaseChannel
rustup target add $Target | Out-Null
cargo build --locked --release -p majik-app --target $Target
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$Binary = "target\$Target\release\majik.exe"

# The same guard script/lib/release.sh applies: prove the stamp landed before packaging. Reading the
# bytes rather than running the binary keeps this working for a GUI subsystem executable.
$Marker = "$ChannelMarkerPrefix$ReleaseChannel"
$Bytes = [System.IO.File]::ReadAllBytes($Binary)
$AsText = [System.Text.Encoding]::ASCII.GetString($Bytes)
if (-not $AsText.Contains($Marker)) {
    throw "FATAL: $Binary was not built with MAJIK_CHANNEL=$ReleaseChannel."
}

$StageDir = "target\inno\$Architecture"
if (Test-Path $StageDir) { Remove-Item -Path $StageDir -Recurse -Force }
New-Item -Path $StageDir -ItemType Directory -Force | Out-Null
Copy-Item -Path $Binary -Destination "$StageDir\Majik.exe" -Force
Copy-Item -Path "packaging\majik.ico" -Destination "$StageDir\majik.ico" -Force
# GPL-3 asks that the licence travel with every binary we hand out; the installer shows
# LICENSE on its own wizard page and drops both next to the executable.
Copy-Item -Path "LICENSE" -Destination "$StageDir\LICENSE.txt" -Force
Copy-Item -Path "NOTICE" -Destination "$StageDir\NOTICE.txt" -Force

$Iscc = "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"
if (-not (Test-Path $Iscc)) {
    throw "Inno Setup not found at $Iscc. Install it with: choco install innosetup -y"
}

$IsccArgs = @(
    "packaging\majik.iss",
    "/dVersion=$Version",
    "/dArch=$Architecture",
    "/dSourceDir=$RepoRoot\$StageDir",
    "/dOutputDir=$RepoRoot\target"
)
Write-Host "Running Inno Setup: $Iscc $IsccArgs"
$Process = Start-Process -FilePath $Iscc -ArgumentList $IsccArgs -NoNewWindow -Wait -PassThru
if ($Process.ExitCode -ne 0) {
    throw "Inno Setup failed with exit code $($Process.ExitCode)"
}

Write-Host ""
Write-Host "Built target\MajikSetup-$Architecture.exe (Majik $Version, $ReleaseChannel)"
