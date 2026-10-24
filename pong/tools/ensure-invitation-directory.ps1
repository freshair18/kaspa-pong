# Restart this workstation's explicitly configured directory, without a wallet.
$ErrorActionPreference = 'Stop'
$directoryRoot = Split-Path -Parent $PSScriptRoot
$directoryConfig = Join-Path $directoryRoot 'node.json'
$directoryState = Join-Path $directoryRoot 'target/invitation-directory-v21.json'
if (!(Test-Path -LiteralPath $directoryConfig)) { exit 0 }
$directorySettings = Get-Content -LiteralPath $directoryConfig -Raw | ConvertFrom-Json
if ($directorySettings.invitation_index -notmatch '^http://127\.0\.0\.1:(\d+)$') { exit 0 }
$directoryPort = [int]$Matches[1]
$directoryArtifact = (Get-Content -LiteralPath (Join-Path $directoryRoot 'artifacts/artifact.json') -Raw | ConvertFrom-Json).id
function Assert-DirectoryVersion {
    # The index serves between bounded accepted-chain batches; an initial
    # catch-up can legitimately take longer than the TCP readiness probe.
    for ($healthAttempt = 0; $healthAttempt -lt 3; $healthAttempt++) {
        try { $health = Invoke-RestMethod -Uri ($directorySettings.invitation_index + '/health') -TimeoutSec 5; break }
        catch {
            if ($healthAttempt -eq 2) { throw ('Cannot reach the configured local invitation directory after three attempts: ' + $_.Exception.Message + ' Check its diagnostics before restarting it; this does not establish a rules/version mismatch.') }
            Start-Sleep -Milliseconds 250
        }
    }
    if ($health.version -ne 21 -or $health.network -ne $directorySettings.network -or $health.artifact_id -ne $directoryArtifact) {
        throw 'The configured local invitation directory has different rules/network. Restart it with the current client and fresh v21 index state.'
    }
}
$directorySocket = [Net.Sockets.TcpClient]::new()
try {
    $connection = $directorySocket.ConnectAsync('127.0.0.1', $directoryPort)
    $directoryListening = $false
    try { $directoryListening = $connection.Wait(300) -and $directorySocket.Connected } catch {}
    if ($directoryListening) { Assert-DirectoryVersion; exit 0 }
} finally { $directorySocket.Dispose() }
$directoryLog = Join-Path $directoryRoot ('target/invitation-directory-' + (Get-Date -Format 'yyyyMMdd-HHmmssfff'))
$directoryArgs = '--config "' + $directoryConfig + '" index --state "' + $directoryState + '" --listen 127.0.0.1:' + $directoryPort
$directoryProcess = Start-Process -FilePath (Join-Path $directoryRoot 'target/debug/pong-node.exe') -ArgumentList $directoryArgs -WorkingDirectory $directoryRoot -WindowStyle Hidden -RedirectStandardOutput ($directoryLog + '.stdout.log') -RedirectStandardError ($directoryLog + '.stderr.log') -PassThru
Write-Host ('Starting local invitation directory (PID ' + $directoryProcess.Id + ').')
for ($attempt = 0; $attempt -lt 40; $attempt++) {
    if ($directoryProcess.HasExited) {
        throw ('Invitation directory stopped: ' + (Get-Content -LiteralPath ($directoryLog + '.stderr.log') -Raw))
    }
    $directorySocket = [Net.Sockets.TcpClient]::new()
    try {
        $connection = $directorySocket.ConnectAsync('127.0.0.1', $directoryPort)
        $directoryListening = $false
        try { $directoryListening = $connection.Wait(100) -and $directorySocket.Connected } catch {}
        if ($directoryListening) { Assert-DirectoryVersion; exit 0 }
    } finally { $directorySocket.Dispose() }
    Start-Sleep -Milliseconds 250
}
Write-Warning ('Directory is still connecting; diagnostics: ' + $directoryLog + '.stderr.log')
