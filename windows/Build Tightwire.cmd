@echo off
rem Builds Tightwire from this source folder (needs Rust and the Visual Studio
rem C++ Build Tools; the script says what to install if they are missing).
rem The result is dist\Tightwire-Windows; run "Install Tightwire.cmd" there.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
pause
