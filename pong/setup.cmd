@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup-win.ps1" %*
exit /b %ERRORLEVEL%
