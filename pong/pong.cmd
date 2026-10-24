@echo off
setlocal
if "%~1"=="" (
  echo Select a wallet explicitly: .\pong WALLET_NAME [host^|join^|send]
  echo Or use: .\pong --wallet FILE [host^|join^|send]
  exit /b 1
)
pushd "%~dp0" || exit /b 1
REM Cargo returns immediately when the executable is already current, but rebuilds
REM it after a source update. Do not silently launch a stale binary after git pull.
cargo build --locked -p kaspa-pong --bin pong-node --target-dir target
if errorlevel 1 (
  popd
  exit /b 1
)
"target\debug\pong-node.exe" %*
set "PONG_EXIT=%ERRORLEVEL%"
popd
exit /b %PONG_EXIT%
