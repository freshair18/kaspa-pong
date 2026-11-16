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
$scratch = Join-Path $target "argent/delegated-reset-$([Guid]::NewGuid().ToString('N'))"
$patch = Join-Path $PSScriptRoot 'argent-delegated-bank-reset.patch'
$patchId = (Get-FileHash -LiteralPath $patch).Hash.Substring(0, 12).ToLowerInvariant()
# Scratch SDK copies share Cargo fingerprints, so isolate different patches.
$compilerTarget = Join-Path $target "argent/compiler-$($revision.Substring(0, 8))-$patchId"
$sdk = Join-Path $scratch 'sdk'
New-Item -ItemType Directory -Path $sdk -Force | Out-Null
foreach ($relativePath in @('Cargo.toml', 'src', 'std', 'crates')) {
    Copy-Item -LiteralPath (Join-Path $source $relativePath) -Destination $sdk -Recurse
}
Push-Location $sdk
try {
    & git apply $patch
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not apply the delegated Bank reset compiler prototype.'
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

$savedOptIn = $env:ARGENT_DELEGATE_PONG_BANK_RESET
$env:ARGENT_DELEGATE_PONG_BANK_RESET = '1'
try {
    Invoke-PongCargo -CargoArguments @(
        'run', '--offline', '--manifest-path', (Join-Path $scratch 'Cargo.toml'),
        '--target-dir', $compilerTarget, '--',
        'build', (Join-Path $PongRoot 'contracts/reusable.ag'), '--out', $out
    )
} finally {
    $env:ARGENT_DELEGATE_PONG_BANK_RESET = $savedOptIn
}
