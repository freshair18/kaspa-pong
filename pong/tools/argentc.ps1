. "$PSScriptRoot/common.ps1"

if ($args.Count -eq 0) {
    Write-Output 'Usage: .\argentc build [contracts/pong.ag] [--out target/argent/pong]'
    Write-Output '       .\argentc inspect [artifacts|target/argent/pong|artifact.json]'
    exit 0
}
if ($args[0] -notin @('build', 'inspect', '--help', '-h')) {
    throw 'Supported commands: build, inspect. Use .\argentc --help for details.'
}

Push-Location $PongRoot
try {
    # Compile against Pong's locked Git SDK, never the mutable sibling checkout.
    Invoke-PongCargo -CargoArguments (@('run', '--quiet', '--locked', '--offline', '-p', 'kaspa-pong', '--no-default-features', '--features', 'tooling', '--bin', 'pong-build', '--target-dir', (Get-PongTargetDir), '--') + $args)
} finally {
    Pop-Location
}
