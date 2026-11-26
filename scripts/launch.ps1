<#
.SYNOPSIS
  Simple Oathkeep preloading launcher: ensures the binary & Agility SDK are ready,
  pre-translates shaders and pre-warms D3D12 pipelines (`oathkeep launch`), and
  then starts the title with the verified Pareto 8 GB VRAM profile so entering
  the game has zero cold-shader stalls or missing-PSO artifacts.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\launch.ps1 -Exe "<path-to-title-exe>"

.EXAMPLE
  # Warm caches only (without starting the title):
  powershell -ExecutionPolicy Bypass -File scripts\launch.ps1 -Exe "<path-to-title-exe>" -NoLaunch
#>
[CmdletBinding()]
param(
    [string]$Exe,
    [ValidateSet('pareto', 'nvidia8gb', 'fast', 'safe')][string]$Profile = 'pareto',
    [ValidateSet('series-s', 'series-x')][string]$Console = 'series-s',
    [int]$Jobs = 4,
    [switch]$Diag,
    [switch]$WarmAll,
    [switch]$ForceWarm,
    [switch]$SkipWarm,
    [switch]$NoLaunch,
    [switch]$Rebuild,
    [string]$Cache = "$env:LOCALAPPDATA\oathkeep\cache-pareto",
    [string]$Data = "$env:LOCALAPPDATA\oathkeep\data"
)

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $repo 'Cargo.toml'
$env:CARGO_BUILD_JOBS = '4'

$binDir = if ($Diag) { Join-Path $repo 'target\diag' } else { Join-Path $repo 'target\release' }
$oathkeepExe = Join-Path $binDir 'oathkeep.exe'

if ($Rebuild -or -not (Test-Path -LiteralPath $oathkeepExe -PathType Leaf)) {
    Push-Location $repo
    try {
        if ($Diag) {
            Write-Host 'Building Oathkeep (diag, diagnostics, -j 4)...'
            & cargo build -j 4 --profile diag -p oathkeep --features diagnostics --manifest-path $manifest
        } else {
            Write-Host 'Building Oathkeep (release, -j 4)...'
            & cargo build -j 4 --release -p oathkeep --manifest-path $manifest
        }
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit code $LASTEXITCODE)." }
    } finally {
        Pop-Location
    }
}

$osBuild = [Environment]::OSVersion.Version.Build
$agilityDll = Join-Path $binDir 'D3D12\D3D12Core.dll'
if ($osBuild -lt 22000 -and -not (Test-Path -LiteralPath $agilityDll)) {
    Write-Host "Windows 10 detected (build $osBuild) without D3D12Core.dll; running setup-agility-sdk.ps1..."
    & (Join-Path $PSScriptRoot 'setup-agility-sdk.ps1')
}

New-Item -ItemType Directory -Force -Path $Cache, $Data | Out-Null
$env:OATHKEEP_CACHE_DIR = $Cache
$env:OATHKEEP_DATA_DIR = $Data
if ($Exe) {
    if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
        throw "Target executable not found: '$Exe'"
    }
    $env:OATHKEEP_GAME_DIR = Split-Path -Parent (Resolve-Path -LiteralPath $Exe).Path
}

$launchArgs = @(
    'launch',
    '--profile', $Profile,
    '--console', $Console,
    '--jobs', "$Jobs"
)
if ($WarmAll)   { $launchArgs += '--warm-all' }
if ($ForceWarm) { $launchArgs += '--force-warm' }
if ($SkipWarm)  { $launchArgs += '--skip-warm' }
if ($NoLaunch)  { $launchArgs += '--no-launch' }
if ($Exe)       { $launchArgs += $Exe }

& $oathkeepExe @launchArgs
exit $LASTEXITCODE
