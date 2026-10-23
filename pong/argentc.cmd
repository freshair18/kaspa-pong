@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0tools\argentc.ps1" %*
exit /b %ERRORLEVEL%
