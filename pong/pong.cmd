@echo off
setlocal
if "%~1"=="" (
  echo Select a wallet explicitly: .\pong WALLET_NAME [host^|join^|send]
  echo Or use: .\pong --wallet FILE [host^|join^|send]
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
"target\debug\pong-node.exe" %*
set "PONG_EXIT=%ERRORLEVEL%"
popd
exit /b %PONG_EXIT%
