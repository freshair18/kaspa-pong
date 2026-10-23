. "$PSScriptRoot/tools/common.ps1"

if ($args.Count -gt 1 -or ($args.Count -eq 1 -and $args[0] -notin @('--vscode-ext', '--help', '-h'))) {
    throw 'Usage: .\setup [--vscode-ext]. No source checkout is cloned or updated.'
}
if ($args.Count -eq 1 -and $args[0] -in @('--help', '-h')) {
    Write-Output 'Usage: .\setup [--vscode-ext]'
    Write-Output 'Reports local tooling without building or testing.'
    Write-Output '--vscode-ext links Argent language support into VS Code (reload afterward).'
    Write-Output 'PONG_ARGENT_DIR selects the editor source checkout; the compiler remains pinned by Cargo.lock.'
    exit 0
}

Push-Location $PongRoot
try {
    & cargo --version
    if ($LASTEXITCODE -ne 0) { throw 'Cargo is unavailable' }
    & rustc --version
    if ($LASTEXITCODE -ne 0) { throw 'Rust is unavailable' }
    Write-Output "Build cache: $(Get-PongTargetDir)"
    Write-Output 'Compiler/runtime: pinned Git revisions in Cargo.toml and Cargo.lock.'

    $ArgentDir = if ($env:PONG_ARGENT_DIR) { [IO.Path]::GetFullPath($env:PONG_ARGENT_DIR) } else { $null }
    if ($ArgentDir -and (Test-Path -LiteralPath (Join-Path $ArgentDir '.git'))) {
        $Revision = & git -C $ArgentDir rev-parse HEAD
        if ($LASTEXITCODE -ne 0) { throw 'Could not inspect local Argent revision' }
        $Changes = @(& git -C $ArgentDir status --porcelain)
        if ($LASTEXITCODE -ne 0) { throw 'Could not inspect local Argent working tree' }
        Write-Output "Optional editor checkout: $ArgentDir ($Revision; $($Changes.Count) changed paths)"
        Write-Output 'Existing checkout is reused without fetching, pulling, switching or changing dependencies.'
    }
    if ($args.Count -eq 0) { exit 0 }
    if (-not $ArgentDir) {
        throw 'Set PONG_ARGENT_DIR to an Argent source checkout to link its optional VS Code extension.'
    }

    $Source = (Resolve-Path -LiteralPath (Join-Path $ArgentDir 'vscode/argent-syntax')).Path
    $Package = Get-Content -LiteralPath (Join-Path $Source 'package.json') -Raw | ConvertFrom-Json
    if (-not $Package.publisher -or -not $Package.name -or -not $Package.version) {
        throw 'The Argent extension manifest has no valid identity'
    }
    $ExtensionsDir = if ($env:VSCODE_EXTENSIONS_DIR) { $env:VSCODE_EXTENSIONS_DIR } else { Join-Path $env:USERPROFILE '.vscode/extensions' }
    $Destination = Join-Path $ExtensionsDir "$($Package.publisher).$($Package.name)-$($Package.version)"
    $Existing = Get-Item -LiteralPath $Destination -Force -ErrorAction SilentlyContinue
    if ($Existing) {
        if ($Existing.LinkType -notin @('Junction', 'SymbolicLink')) { throw "Refusing to replace existing extension: $Destination" }
        $ExistingTarget = [string]$Existing.Target
        if (-not [IO.Path]::IsPathRooted($ExistingTarget)) { $ExistingTarget = Join-Path $ExtensionsDir $ExistingTarget }
        if (-not [string]::Equals([IO.Path]::GetFullPath($ExistingTarget).TrimEnd('\'), $Source.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to replace extension linked elsewhere: $Destination"
        }
        Write-Output "Already linked: $Destination -> $Source"
    } else {
        New-Item -ItemType Directory -Path $ExtensionsDir -Force | Out-Null
        New-Item -ItemType Junction -Path $Destination -Target $Source | Out-Null
        Write-Output "Linked: $Destination -> $Source"
    }
    Write-Output 'Reload VS Code and open contracts/pong.ag. Uninstall this development link only after exiting VS Code; do not uninstall it through the Extensions view.'
} finally {
    Pop-Location
}
