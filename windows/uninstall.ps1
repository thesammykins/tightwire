# Tightwire uninstaller. UNTESTED ON WINDOWS.
param(
    [string]$UserLocalAppData = $env:LOCALAPPDATA,
    [string]$UserAppData = $env:APPDATA,
    [string]$UserName = $env:USERNAME
)
$ErrorActionPreference = 'Stop'
$me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"",
        '-UserLocalAppData', "`"$UserLocalAppData`"", '-UserAppData', "`"$UserAppData`"", '-UserName', "`"$UserName`"")
    $p = Start-Process powershell -Verb RunAs -ArgumentList $argList -Wait -PassThru
    exit $p.ExitCode
}
$InstallDir = Join-Path $UserLocalAppData 'Programs\Tightwire'
Write-Host 'Stopping Tightwire'
Get-Process -Name tightwire -ErrorAction SilentlyContinue | Stop-Process -Force
Write-Host 'Removing firewall rule'
Get-NetFirewallRule -DisplayName 'Tightwire (UDP)' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $UserAppData 'Microsoft\Windows\Start Menu\Programs\Tightwire.lnk')
if ($env:USERNAME -eq $UserName) {
    Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name Tightwire -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Tightwire'
}
$keep = Read-Host 'Also delete the pairing and logs? [y/N]'
if ($keep -match '^(y|yes)$') {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue (Join-Path $UserAppData 'Tightwire'), (Join-Path $UserLocalAppData 'Tightwire')
}
Write-Host "Removing $InstallDir"
Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $InstallDir
Write-Host 'Tightwire removed. usbip-win2 was left installed (remove it from Apps if unused).'
Read-Host 'Press Enter to close'
