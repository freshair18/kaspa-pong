param(
    [Parameter(Mandatory = $true)][string]$ArgentSource,
    [string]$OutputDirectory
)

. "$PSScriptRoot/common.ps1"

$revision = '9a9f4b107116d0b259fae629f200fa3b663e7e9d'
$source = (Resolve-Path -LiteralPath $ArgentSource).Path
$head = & git -C $source rev-parse HEAD
if ($LASTEXITCODE -ne 0 -or $head -ne $revision) {
    throw "ArgentSource must be checked out at $revision."
}
if (& git -C $source status --porcelain --untracked-files=no) {
    throw 'ArgentSource must have clean tracked files.'
}
if ($LASTEXITCODE -ne 0) {
    throw 'Could not check the Argent checkout.'
}

$target = Get-PongTargetDir
if (!$OutputDirectory) {
    $OutputDirectory = Join-Path $target 'argent/reusable'
}
$out = [IO.Path]::GetFullPath($OutputDirectory)
$scratch = Join-Path $target "argent/shared-bank-$([Guid]::NewGuid().ToString('N'))"
$sdk = Join-Path $scratch 'sdk'
New-Item -ItemType Directory -Path $sdk -Force | Out-Null
foreach ($relativePath in @('Cargo.toml', 'src', 'std', 'crates')) {
    Copy-Item -LiteralPath (Join-Path $source $relativePath) -Destination $sdk -Recurse
}
Push-Location $sdk
try {
    & git apply (Join-Path $PSScriptRoot 'argent-shared-bank.patch')
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not apply the shared Bank compiler patch.'
    }
} finally {
    Pop-Location
}

# Reuse the checked-in Pong dependency resolution in the scratch builder.
Copy-Item -LiteralPath (Join-Path $PongRoot 'Cargo.lock') -Destination $scratch
Copy-Item -LiteralPath (Join-Path $PongRoot 'src/build.rs') -Destination (Join-Path $scratch 'build.rs')
@'
[package]
name = "pong-artifact-compiler"
version = "0.1.0"
edition = "2024"
build = false

[[bin]]
name = "pong-artifact-compiler"
path = "build.rs"

[dependencies]
argent = { path = "sdk" }
clap = { version = "4.6.1", features = ["derive"] }
serde_json = "1"
'@ | Set-Content -Encoding ascii -LiteralPath (Join-Path $scratch 'Cargo.toml')

$savedOptIn = $env:ARGENT_SHARE_PONG_BANK_TEMPLATE
$env:ARGENT_SHARE_PONG_BANK_TEMPLATE = '1'
try {
    Invoke-PongCargo -CargoArguments @(
        'run', '--offline', '--manifest-path', (Join-Path $scratch 'Cargo.toml'),
        '--target-dir', $target, '--',
        'build', (Join-Path $PongRoot 'contracts/reusable.ag'), '--out', $out
    )
} finally {
    $env:ARGENT_SHARE_PONG_BANK_TEMPLATE = $savedOptIn
}
