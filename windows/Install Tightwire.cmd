@echo off
rem Installs or updates Tightwire for the current user. Safe to run again.
rem Passes this user's folders to the elevated installer, because an
rem administrator prompt may run as a different account.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" -UserLocalAppData "%LOCALAPPDATA%" -UserAppData "%APPDATA%" -UserName "%USERNAME%" -Source "%~dp0."
if errorlevel 1 pause
