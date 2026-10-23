$ErrorActionPreference = 'Stop'
$PongRoot = Split-Path -Parent $PSScriptRoot

function Get-PongTargetDir {
    if ($env:CARGO_TARGET_DIR) {
        return [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
    }
    return Join-Path $PongRoot 'target'
}

function Invoke-PongCargo {
    param([string[]]$CargoArguments)
    & cargo @CargoArguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo failed with exit code $LASTEXITCODE"
    }
}
