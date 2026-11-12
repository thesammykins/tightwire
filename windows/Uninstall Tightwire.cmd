@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0uninstall.ps1" -UserLocalAppData "%LOCALAPPDATA%" -UserAppData "%APPDATA%" -UserName "%USERNAME%"
if errorlevel 1 pause
