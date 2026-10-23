@echo off
setlocal
if "%~1"=="" (
  echo Select a wallet explicitly: .\pong WALLET_NAME [host^|join]
  echo Or use: .\pong --wallet FILE [host^|join]
  exit /b 1
)
pushd "%~dp0" || exit /b 1
if not exist "target\debug\pong-node.exe" (
  cargo build --locked -p kaspa-pong --bin pong-node --target-dir target
  if errorlevel 1 (
    popd
    exit /b 1
  )
)
rem Custom configurations and directory/operator commands remain explicit.
if /i "%~1"=="--config" goto run_client
if /i "%~1"=="index" goto run_client
if /i "%~1"=="probe" goto run_client
if /i "%~1"=="keygen" goto run_client
if /i "%~1"=="address" goto run_client
powershell.exe -NoProfile -File "%~dp0tools\ensure-invitation-directory.ps1"
:run_client
"target\debug\pong-node.exe" %*
set "PONG_EXIT=%ERRORLEVEL%"
popd
exit /b %PONG_EXIT%
