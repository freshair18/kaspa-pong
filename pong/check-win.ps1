. "$PSScriptRoot/tools/common.ps1"

if ($args.Count -eq 0 -or $args[0] -in @('--help', '-h')) {
    Write-Output 'Usage: .\check fmt                # check Rust formatting'
    Write-Output '       .\check lint               # run Clippy on default targets'
    Write-Output '       .\check node FILTER        # wallet and client settings'
    Write-Output '       .\check gameplay FILTER    # physics and animation'
    Write-Output '       .\check ui FILTER          # controls and messages'
    Write-Output '       .\check unit FILTER        # library tests (e.g. menu::tests)'
    Write-Output '       .\check contract FILTER    # covenant artifact reproduction'
    Write-Output 'Test commands require a test name or module filter.'
    exit 0
}
if ($args.Count -eq 1 -and $args[0] -in @('fmt', 'lint')) {
    $CargoArguments = if ($args[0] -eq 'fmt') {
        @('fmt', '-p', 'kaspa-pong', '--', '--check')
    } else {
        @('clippy', '--locked', '--offline', '-p', 'kaspa-pong', '--all-targets', '--target-dir', (Get-PongTargetDir), '--', '-D', 'warnings')
    }
    Push-Location $PongRoot
    try {
        Invoke-PongCargo -CargoArguments $CargoArguments
    } finally {
        Pop-Location
    }
    exit 0
}
if ($args.Count -ne 2 -or [string]::IsNullOrWhiteSpace($args[1]) -or $args[1].StartsWith('-')) {
    throw 'Specify exactly one test target and a nonempty test/module filter; use .\check --help.'
}
$Target = switch ($args[0]) {
    'node' { 'node' }
    'gameplay' { 'gameplay' }
    'ui' { 'ui' }
    'unit' { 'lib' }
    'contract' { 'contract' }
    default { throw 'Unknown target. Choose node, gameplay, ui, unit or contract.' }
}
$CargoArguments = @('test', '--locked', '--offline', '-p', 'kaspa-pong', '--target-dir', (Get-PongTargetDir))
if ($Target -eq 'lib') { $CargoArguments += '--lib' }
else { $CargoArguments += @('--test', $Target) }
if ($Target -eq 'contract') {
    $CargoArguments += @('--no-default-features', '--features', 'tooling')
}
$Filter = $args[1]
$CargoArguments += @($Filter, '--', '--nocapture')
if ($Target -eq 'contract') { $CargoArguments += '--ignored' }
Push-Location $PongRoot
try {
    Invoke-PongCargo -CargoArguments $CargoArguments
} finally {
    Pop-Location
}
