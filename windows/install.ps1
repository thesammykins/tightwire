# Tightwire installer (SPEC section 10). Idempotent: re-run to upgrade.
# UNTESTED ON WINDOWS: written and syntax-reviewed on macOS only.
param(
    [string]$UserLocalAppData = $env:LOCALAPPDATA,
    [string]$UserAppData = $env:APPDATA,
    [string]$UserName = $env:USERNAME,
    [string]$Source = $PSScriptRoot
)
$ErrorActionPreference = 'Stop'
$RuleName = 'Tightwire (UDP)'
$Ports = @(4810, 4814)   # audio/control, pairing
$UsbipWinget = 'vadimgrn.usbip-win2'

$me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'Tightwire needs administrator rights once, to add a firewall rule. Approve the prompt.'
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"",
        '-UserLocalAppData', "`"$UserLocalAppData`"", '-UserAppData', "`"$UserAppData`"",
        '-UserName', "`"$UserName`"", '-Source', "`"$Source`"")
    $p = Start-Process powershell -Verb RunAs -ArgumentList $argList -Wait -PassThru
    exit $p.ExitCode
}

$InstallDir = Join-Path $UserLocalAppData 'Programs\Tightwire'
$LogDir = Join-Path $UserLocalAppData 'Tightwire'
New-Item -ItemType Directory -Force -Path $InstallDir, $LogDir | Out-Null
Start-Transcript -Path (Join-Path $LogDir 'install.log') -Append | Out-Null
$sameUser = ($env:USERNAME -eq $UserName)
try {
    Write-Host "[1/7] Stopping a running Tightwire (if any)"
    Get-Process -Name tightwire -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500

    Write-Host "[2/7] Copying to $InstallDir"
    foreach ($f in 'tightwire.exe', 'uninstall.ps1', 'Uninstall Tightwire.cmd', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt') {
        Copy-Item -Force -LiteralPath (Join-Path $Source $f) -Destination $InstallDir
    }
    $Exe = Join-Path $InstallDir 'tightwire.exe'

    Write-Host "[3/7] Firewall: allow UDP $($Ports -join ',') from the local subnet"
    Get-NetFirewallRule -DisplayName $RuleName -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    New-NetFirewallRule -DisplayName $RuleName -Direction Inbound -Action Allow -Protocol UDP `
        -LocalPort $Ports -RemoteAddress LocalSubnet -Profile Private,Domain -Program $Exe | Out-Null

    Write-Host "[4/7] Stream Deck handoff: looking for usbip-win2"
    $usbip = 'C:\Program Files\USBip\usbip.exe'
    if (-not (Test-Path $usbip)) {
        $cmd = Get-Command usbip.exe -ErrorAction SilentlyContinue
        if ($cmd) { $usbip = $cmd.Source } else { $usbip = $null }
    }
    if ($usbip) {
        Write-Host "      found $usbip"
    } else {
        Write-Host '      usbip-win2 is not installed. It is only needed to move the Stream Deck between Mac and PC.'
        $answer = Read-Host '      Install it now with winget (you will see its licence terms)? [y/N]'
        if ($answer -match '^(y|yes)$') {
            if (Get-Command winget -ErrorAction SilentlyContinue) {
                winget install --id $UsbipWinget -e
                if ($LASTEXITCODE -ne 0) { Write-Warning "winget exited with $LASTEXITCODE" }
            } else {
                Write-Warning 'winget is not available. Install usbip-win2 from https://github.com/vadimgrn/usbip-win2/releases'
            }
        }
    }

    Write-Host "[5/7] Start menu shortcut"
    $menu = Join-Path $UserAppData 'Microsoft\Windows\Start Menu\Programs'
    $lnk = (New-Object -ComObject WScript.Shell).CreateShortcut((Join-Path $menu 'Tightwire.lnk'))
    $lnk.TargetPath = $Exe
    $lnk.WorkingDirectory = $InstallDir
    $lnk.Description = 'Tightwire receiver (tray)'
    $lnk.Save()

    Write-Host "[6/7] Uninstaller and start at login"
    if ($sameUser) {
        $key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Tightwire'
        New-Item -Force -Path $key | Out-Null
        $uninst = "`"$InstallDir\Uninstall Tightwire.cmd`""
        Set-ItemProperty -Path $key -Name DisplayName -Value 'Tightwire'
        Set-ItemProperty -Path $key -Name UninstallString -Value $uninst
        Set-ItemProperty -Path $key -Name InstallLocation -Value $InstallDir
        Set-ItemProperty -Path $key -Name DisplayIcon -Value $Exe
        Set-ItemProperty -Path $key -Name Publisher -Value 'Tightwire'
        Set-ItemProperty -Path $key -Name NoModify -Value 1 -Type DWord
        Set-ItemProperty -Path $key -Name NoRepair -Value 1 -Type DWord
        $login = Read-Host '      Start Tightwire when you sign in? [Y/n]'
        $run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
        if ($login -notmatch '^(n|no)$') {
            Set-ItemProperty -Path $run -Name Tightwire -Value "`"$Exe`""
        } else {
            Remove-ItemProperty -Path $run -Name Tightwire -ErrorAction SilentlyContinue
        }
    } else {
        Write-Warning "The administrator prompt ran as $env:USERNAME, not $UserName."
        Write-Warning "Use 'Start at login' in the tray menu, and '$InstallDir\Uninstall Tightwire.cmd' to remove."
    }

    Write-Host "[7/7] Starting Tightwire"
    # explorer.exe launches it as the signed-in user, not elevated.
    Start-Process explorer.exe -ArgumentList "`"$Exe`""
    Write-Host ''
    Write-Host 'Done. Right-click the Tightwire icon in the notification area and choose Pair...'
    Write-Host 'Then choose Output device: a dedicated virtual endpoint (a Wave Link channel, or "CABLE Input").'
    Write-Host 'Tightwire plays nothing until you choose, and never uses the system default output.'
    Write-Host "Diagnostics: `"$Exe`" doctor"
} catch {
    Write-Error "Install failed: $_"
    Stop-Transcript | Out-Null
    exit 1
}
Stop-Transcript | Out-Null
Read-Host 'Press Enter to close'
