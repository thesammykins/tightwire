# Builds the Tightwire receiver natively on Windows and lays it out exactly
# like Tightwire-Windows.zip (the Mac's `mise run package-windows`), so
# "Install Tightwire.cmd" works from the result:
#   dist\Tightwire-Windows\   tightwire.exe, installer scripts, README.txt,
#                             LICENSE.txt, THIRD-PARTY-NOTICES.txt
# Needs Rust (rustup, MSVC toolchain) and the Visual Studio C++ Build Tools.
# It checks for both and says what to install; it never installs anything.
# Usage: double-click build.cmd, or  powershell -ExecutionPolicy Bypass -File windows\build.ps1
# Plain ASCII on purpose: Windows PowerShell 5.1 reads BOM-less files as ANSI.
param(
    # Output folder (default: <repository>\dist\Tightwire-Windows).
    [string]$OutDir
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3

$Root = Split-Path -Parent $PSScriptRoot
$Manifest = Join-Path $Root 'receiver\Cargo.toml'
if (-not $OutDir) { $OutDir = Join-Path $Root 'dist\Tightwire-Windows' }
$Target = 'x86_64-pc-windows-msvc'

function Step([string]$text) { Write-Host ''; Write-Host "== $text" }
# Native tools (cargo, rustc) write progress to stderr; Windows PowerShell 5.1
# can turn that into terminating errors under 'Stop'. Their exit codes are
# checked instead.
function Native([scriptblock]$block) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $block } finally { $ErrorActionPreference = $saved }
}
function Fail([string[]]$lines) {
    Write-Host ''
    foreach ($l in $lines) { Write-Host $l -ForegroundColor Red }
    exit 1
}

if (-not (Test-Path -LiteralPath $Manifest)) {
    Fail @("Cannot find $Manifest.", 'Run this script from the windows folder of a Tightwire source checkout.')
}
Write-Host "Tightwire Windows build"
Write-Host "  source: $Root"
Write-Host "  output: $OutDir"
Write-Host "  PowerShell $($PSVersionTable.PSVersion) on $([Environment]::OSVersion.VersionString)"

Step '[1/5] Rust'
# rustup puts cargo in %USERPROFILE%\.cargo\bin; a terminal opened before the
# install does not have it on PATH yet.
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path -LiteralPath (Join-Path $cargoBin 'cargo.exe')) {
        Write-Host "  cargo is not on PATH; using $cargoBin"
        $env:Path = "$cargoBin;$env:Path"
    }
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue) -or -not (Get-Command rustc -ErrorAction SilentlyContinue)) {
    Fail @(
        'Rust is not installed (no cargo/rustc found).',
        'Install it, then run this script again in a NEW terminal window:',
        '    winget install --id Rustlang.Rustup -e',
        'or download rustup-init.exe from https://rustup.rs and accept the defaults.'
    )
}
$rustcInfo = (Native { rustc -vV }) -join "`n"
if ($LASTEXITCODE -ne 0) { Fail @('rustc -vV failed; run "rustup default stable" and try again.') }
Write-Host ($rustcInfo -split "`n" | Select-Object -First 1)
$hostLine = ($rustcInfo -split "`n" | Where-Object { $_ -like 'host:*' } | Select-Object -First 1)
$rustHost = if ($hostLine) { $hostLine.Substring(5).Trim() } else { 'unknown' }
Write-Host "  host: $rustHost"
if ($rustHost -ne $Target) {
    Fail @(
        "Rust's default toolchain is for $rustHost; Tightwire is built for $Target.",
        'Switch the default toolchain, then run this script again:',
        '    rustup default stable-x86_64-pc-windows-msvc'
    )
}

Step '[2/5] Visual Studio C++ Build Tools (the MSVC linker and Windows SDK)'
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vsPath = $null
if (Test-Path -LiteralPath $vswhere) {
    $vsPath = Native { & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath }
}
if ($vsPath) {
    Write-Host "  found $vsPath"
} elseif (Get-Command link.exe -ErrorAction SilentlyContinue) {
    # A Developer Command Prompt (or a custom setup) already has the linker on PATH.
    Write-Host "  vswhere found no C++ tools, but link.exe is on PATH: $((Get-Command link.exe).Source)"
} else {
    Fail @(
        'The Visual Studio C++ Build Tools are not installed; Rust needs their linker.',
        'Install them (about 2-3 GB), then run this script again:',
        '    winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--passive --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"',
        'or run the Visual Studio Installer and add the "Desktop development with C++" workload.'
    )
}

Step '[3/5] cargo build --release (first build downloads crates and takes a few minutes)'
Native { cargo build --release --locked --manifest-path $Manifest --bin tightwire-recv }
if ($LASTEXITCODE -ne 0) { Fail @("cargo build failed (exit $LASTEXITCODE); see the errors above.") }
$metaJson = Native { cargo metadata --format-version 1 --no-deps --manifest-path $Manifest }
if ($LASTEXITCODE -ne 0) { Fail @('cargo metadata failed.') }
$meta = ($metaJson -join "`n") | ConvertFrom-Json
$exe = Join-Path $meta.target_directory 'release\tightwire-recv.exe'
if (-not (Test-Path -LiteralPath $exe)) { Fail @("Build finished but $exe is missing.") }
Write-Host "  built $exe"

Step "[4/5] Assembling $OutDir"
if (Test-Path -LiteralPath $OutDir) { Remove-Item -Recurse -Force -LiteralPath $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
# Same files, same names as the zip from scripts/package-windows.sh.
$files = @(
    @{ From = $exe; To = 'tightwire.exe' },
    @{ From = (Join-Path $PSScriptRoot 'Install Tightwire.cmd'); To = 'Install Tightwire.cmd' },
    @{ From = (Join-Path $PSScriptRoot 'Uninstall Tightwire.cmd'); To = 'Uninstall Tightwire.cmd' },
    @{ From = (Join-Path $PSScriptRoot 'install.ps1'); To = 'install.ps1' },
    @{ From = (Join-Path $PSScriptRoot 'uninstall.ps1'); To = 'uninstall.ps1' },
    @{ From = (Join-Path $PSScriptRoot 'README.txt'); To = 'README.txt' },
    @{ From = (Join-Path $Root 'LICENSE'); To = 'LICENSE.txt' },
    # The committed notices (the Mac build regenerates them with cargo-about,
    # which this script does not need).
    @{ From = (Join-Path $Root 'THIRD-PARTY-NOTICES.txt'); To = 'THIRD-PARTY-NOTICES.txt' }
)
foreach ($f in $files) {
    if (-not (Test-Path -LiteralPath $f.From)) { Fail @("Missing $($f.From); is the checkout complete?") }
    Copy-Item -Force -LiteralPath $f.From -Destination (Join-Path $OutDir $f.To)
    Write-Host "  $($f.To)"
}

Step '[5/5] Checking the exe'
# GUI-subsystem binary: Start-Process is the reliable way to wait for it and
# capture its output.
$out = Join-Path $env:TEMP "tightwire-build-version-$PID.txt"
$p = Start-Process -FilePath (Join-Path $OutDir 'tightwire.exe') -ArgumentList '--version' -NoNewWindow -PassThru -RedirectStandardOutput $out
# Windows PowerShell 5.1 only reports ExitCode for a -PassThru process if its
# handle was read while it was alive; without this ExitCode is empty.
$null = $p.Handle
if (-not $p.WaitForExit(30000)) { $p.Kill(); Fail @('tightwire.exe --version did not finish within 30 s.') }
$version = [string](Get-Content -Raw -LiteralPath $out -ErrorAction SilentlyContinue)
Remove-Item -Force -LiteralPath $out -ErrorAction SilentlyContinue
if ($p.ExitCode -ne 0) { Fail @("tightwire.exe --version exited with $($p.ExitCode).") }
Write-Host "  $($version.Trim())"

Write-Host ''
Write-Host 'Done. To install, open this folder and double-click "Install Tightwire.cmd":' -ForegroundColor Green
Write-Host "  $OutDir"
