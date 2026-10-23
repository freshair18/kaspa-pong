# Restart this workstation's explicitly configured directory, without a wallet.
$ErrorActionPreference = 'Stop'
$directoryRoot = Split-Path -Parent $PSScriptRoot
$directoryConfig = Join-Path $directoryRoot 'node.json'
$directoryState = Join-Path $directoryRoot 'target/invitation-directory-v19.json'
if (!(Test-Path -LiteralPath $directoryConfig) -or !(Test-Path -LiteralPath $directoryState)) { exit 0 }
$directorySettings = Get-Content -LiteralPath $directoryConfig -Raw | ConvertFrom-Json
if ($directorySettings.invitation_index -ne 'http://127.0.0.1:17720') { exit 0 }
$directorySocket = [Net.Sockets.TcpClient]::new()
try {
    $connection = $directorySocket.ConnectAsync('127.0.0.1', 17720)
    try { if ($connection.Wait(300) -and $directorySocket.Connected) { exit 0 } } catch {}
} finally { $directorySocket.Dispose() }
$directoryLog = Join-Path $directoryRoot ('target/invitation-directory-' + (Get-Date -Format 'yyyyMMdd-HHmmssfff'))
$directoryArgs = '--config "' + $directoryConfig + '" index --state "' + $directoryState + '" --listen 127.0.0.1:17720'
$directoryProcess = Start-Process -FilePath (Join-Path $directoryRoot 'target/debug/pong-node.exe') -ArgumentList $directoryArgs -WorkingDirectory $directoryRoot -WindowStyle Hidden -RedirectStandardOutput ($directoryLog + '.stdout.log') -RedirectStandardError ($directoryLog + '.stderr.log') -PassThru
Write-Host ('Starting local invitation directory (PID ' + $directoryProcess.Id + ').')
for ($attempt = 0; $attempt -lt 40; $attempt++) {
    if ($directoryProcess.HasExited) {
        throw ('Invitation directory stopped: ' + (Get-Content -LiteralPath ($directoryLog + '.stderr.log') -Raw))
    }
    $directorySocket = [Net.Sockets.TcpClient]::new()
    try {
        $connection = $directorySocket.ConnectAsync('127.0.0.1', 17720)
        try { if ($connection.Wait(100) -and $directorySocket.Connected) { exit 0 } } catch {}
    } finally { $directorySocket.Dispose() }
    Start-Sleep -Milliseconds 250
}
Write-Warning ('Directory is still connecting; diagnostics: ' + $directoryLog + '.stderr.log')
