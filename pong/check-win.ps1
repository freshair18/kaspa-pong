. "$PSScriptRoot/tools/common.ps1"

if ($args.Count -eq 0 -or $args[0] -in @('--help', '-h')) {
    Write-Output 'Usage: .\check node FILTER        # client/tracker and invitations'
    Write-Output '       .\check gameplay FILTER    # physics and animation'
    Write-Output '       .\check ui FILTER          # controls and messages'
    Write-Output '       .\check unit FILTER        # library tests (e.g. menu::tests)'
    Write-Output '       .\check rpc FILTER         # isolated native-node scenario (requires PONG_KASPAD_BIN)'
    Write-Output '       .\check contract FILTER    # covenant invariants and timeouts'
    Write-Output '       .\check consensus FILTER   # native consensus (opt-in)'
    Write-Output 'A test name or module filter is required; no checks run by default.'
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
    'rpc' { 'node' }
    'contract' { 'contract' }
    'consensus' { 'consensus' }
    default { throw 'Unknown target; choose node, gameplay, ui, unit, rpc, contract or consensus.' }
}
$CargoArguments = @('test', '--locked', '--offline', '-p', 'kaspa-pong', '--target-dir', (Get-PongTargetDir))
if ($Target -eq 'lib') { $CargoArguments += '--lib' }
else { $CargoArguments += @('--test', $Target) }
if ($Target -eq 'contract') {
    $CargoArguments += @('--no-default-features', '--features', 'tooling')
} elseif ($Target -eq 'consensus') {
    $CargoArguments += @('--no-default-features', '--features', 'consensus')
}
$Filter = $args[1]
if ($args[0] -eq 'rpc' -and -not $Filter.StartsWith('rpc::')) { $Filter = "rpc::$Filter" }
$CargoArguments += @($Filter, '--', '--nocapture')
if ($args[0] -eq 'rpc') { $CargoArguments += '--ignored' }
Push-Location $PongRoot
try {
    Invoke-PongCargo -CargoArguments $CargoArguments
} finally {
    Pop-Location
}
