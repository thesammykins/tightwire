# Headless smoke test of the Windows receiver's CLI paths (CI runners have no
# audio endpoints, no pairing, no usbip-win2 and no tray). Each command must
# finish within its timeout and must not crash or panic; an ordinary error
# exit (1, with a message) is allowed where the runner lacks the hardware.
#   pwsh scripts/windows-smoke.ps1 -Exe path\to\tightwire-recv.exe
param([Parameter(Mandatory)] [string] $Exe)
$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path $Exe).Path
$work = Join-Path ([IO.Path]::GetTempPath()) "tightwire-smoke-$PID"
New-Item -ItemType Directory -Force -Path $work | Out-Null
# Keep pairing/config files away from the runner profile.
$config = Join-Path $work 'pairing.conf'

$cases = @(
    @{ Name = 'version';      Args = @('--version');                          Ok = @(0) },
    @{ Name = 'help';         Args = @('--help');                             Ok = @(0) },
    @{ Name = 'show';         Args = @('--config', $config, 'show');           Ok = @(0) },
    @{ Name = 'list-devices'; Args = @('--config', $config, '--list-devices'); Ok = @(0, 1) },
    @{ Name = 'list-cpal';    Args = @('--config', $config, '--backend', 'cpal', '--list-devices'); Ok = @(0, 1) },
    @{ Name = 'doctor';       Args = @('--config', $config, 'doctor');         Ok = @(0) }
)

$failed = @()
foreach ($c in $cases) {
    $out = Join-Path $work "$($c.Name).out"
    $err = Join-Path $work "$($c.Name).err"
    Write-Host "::group::tightwire $($c.Args -join ' ')"
    # The exe is a GUI-subsystem binary: Start-Process -Wait is the reliable
    # way to wait for it and capture its standard streams.
    $p = Start-Process -FilePath $Exe -ArgumentList $c.Args -NoNewWindow -PassThru `
        -RedirectStandardOutput $out -RedirectStandardError $err
    # Read the handle now so ExitCode is populated (PowerShell 5.1 quirk).
    $null = $p.Handle
    if (-not $p.WaitForExit(120000)) {
        $p.Kill()
        $code = 'timeout'
    } else {
        $code = $p.ExitCode
    }
    Get-Content $out -ErrorAction SilentlyContinue | Write-Host
    $stderr = Get-Content $err -Raw -ErrorAction SilentlyContinue
    if ($stderr) { Write-Host "stderr:"; Write-Host $stderr }
    Write-Host "exit code: $code"
    Write-Host '::endgroup::'

    if ($code -eq 'timeout') {
        $failed += "$($c.Name): did not finish within 120 s"
    } elseif ($code -eq 101 -or $stderr -match 'panicked at') {
        $failed += "$($c.Name): Rust panic (exit $code)"
    } elseif ($code -lt 0 -or $code -gt 255) {
        $failed += "$($c.Name): crashed with NTSTATUS 0x{0:X8}" -f $code
    } elseif ($c.Ok -notcontains $code) {
        $failed += "$($c.Name): exit code $code (expected $($c.Ok -join ' or '))"
    } elseif ($code -ne 0) {
        Write-Host "::warning title=$($c.Name)::exit $code on a runner without audio hardware: $($stderr.Trim())"
    }
}

if ($failed.Count) {
    $failed | ForEach-Object { Write-Host "::error title=Windows smoke test::$_" }
    exit 1
}
Write-Host "All $($cases.Count) CLI smoke cases passed."
