@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0check-win.ps1" %*
exit /b %ERRORLEVEL%
