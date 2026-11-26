<#
.SYNOPSIS
  Builds (when needed, capped at -j 4) and launches Oathkeep on a user-supplied
  Xbox Series X|S title executable. Keeps the title directory strictly read-only
  and stores all caches/saves outside the repository.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [ValidateSet('safe', 'fast', 'nvidia8gb', 'pareto')][string]$Profile = 'pareto',
    [ValidateSet('auto', 'series-x', 'series-s')][string]$Console = 'series-s',
    [switch]$Diag,
    [switch]$Dred,
    [string]$Cache = "$env:LOCALAPPDATA\oathkeep\cache",
    [string]$Data = "$env:LOCALAPPDATA\oathkeep\data"
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
    throw "Target executable not found: '$Exe'"
}

$repo = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $repo 'Cargo.toml'
$env:CARGO_BUILD_JOBS = '4'

Push-Location $repo
try {
    if ($Diag) {
        Write-Host 'Building Oathkeep (diag, diagnostics, -j 4)...'
        & cargo build -j 4 --profile diag -p oathkeep --features diagnostics --manifest-path $manifest
        $binDir = Join-Path $repo 'target\diag'
    } else {
        Write-Host 'Building Oathkeep (release, -j 4)...'
        & cargo build -j 4 --release -p oathkeep --manifest-path $manifest
        $binDir = Join-Path $repo 'target\release'
    }
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit code $LASTEXITCODE)." }
} finally {
    Pop-Location
}

$oathkeepExe = Join-Path $binDir 'oathkeep.exe'

$osBuild = [Environment]::OSVersion.Version.Build
$agilityDll = Join-Path $binDir 'D3D12\D3D12Core.dll'
if ($osBuild -lt 22000 -and -not (Test-Path -LiteralPath $agilityDll)) {
    Write-Host "Windows 10 detected (build $osBuild) without D3D12Core.dll; running setup-agility-sdk.ps1..."
    & (Join-Path $PSScriptRoot 'setup-agility-sdk.ps1')
}

foreach ($v in @(
    'OATHKEEP_KEYS', 'OATHKEEP_CAPTURE', 'OATHKEEP_CAPTURE_FRAMES', 'OATHKEEP_DUMP_TARGETS',
    'OATHKEEP_LATE_WAIT', 'OATHKEEP_WIDE_BACK', 'OATHKEEP_WIDE_REFRESH', 'OATHKEEP_WIDE_MERGE',
    'OATHKEEP_JOIN_COPIES', 'OATHKEEP_CARD_REHASH_RUNS', 'OATHKEEP_RELAY',
    'OATHKEEP_CARD_BUFFERS_MIN_MB', 'OATHKEEP_CARD_BUFFERS_MB', 'OATHKEEP_SHADER_LOOP_LIMIT',
    'OATHKEEP_LEVELS_ON_DEMAND', 'OATHKEEP_LEVELS_MIN_KB', 'OATHKEEP_TEXTURE_NO_WAIT',
    'OATHKEEP_PAGE_WRITES', 'OATHKEEP_COMMITTED_QUERY', 'OATHKEEP_INDIRECT_WINDOW',
    'OATHKEEP_TEXTURE_CATCHUP', 'OATHKEEP_TEXTURE_STAGE_MS', 'OATHKEEP_TEXTURE_STAGE_MB',
    'OATHKEEP_BAKE_TEXTURES_MS', 'OATHKEEP_INPLACE', 'OATHKEEP_INPLACE_MB',
    'OATHKEEP_INPLACE_MIN_KB', 'OATHKEEP_INPLACE_MIN_MB', 'OATHKEEP_INPLACE_SECTIONS',
    'OATHKEEP_INPLACE_SECTIONS_MB', 'OATHKEEP_INPLACE_SECTION_KB',
    'OATHKEEP_INPLACE_SECTION_MB', 'OATHKEEP_INPLACE_SECTION_MIN_MB',
    'OATHKEEP_INPLACE_COMMIT_PRIVATE', 'OATHKEEP_INPLACE_COMMIT_PRIVATE_MB',
    'OATHKEEP_DISPLAY_MODE',
    'OATHKEEP_D3D12_DRED', 'OATHKEEP_PIPELINE_WAIT_MS', 'OATHKEEP_WINDOW_KB',
    'OATHKEEP_WINDOW_TAILS', 'OATHKEEP_TITLE_MEMORY_MB', 'OATHKEEP_SPILL_SHADERS',
    'OATHKEEP_PIPELINE_WORKERS', 'OATHKEEP_EARLY_PIPELINE_COMMIT_MB',
    'OATHKEEP_COMMIT_HEADROOM_MB', 'OATHKEEP_DEMAND_COMMIT', 'OATHKEEP_DEMAND_PRIVATE',
    'OATHKEEP_CAPTURE_WORLD', 'OATHKEEP_CARD_ACCOUNTING',
    'OATHKEEP_AMBIENT', 'OATHKEEP_AMBIENT_AIR', 'OATHKEEP_RELAY_COPY',
    'OATHKEEP_CARD_STEPS', 'OATHKEEP_INDIRECT_BATCH', 'OATHKEEP_MAP_THREADS',
    'OATHKEEP_CARD_AWAKE', 'OATHKEEP_SHOW_AFTER', 'OATHKEEP_THIN',
    'OATHKEEP_JOIN_PARTS', 'OATHKEEP_UPLOAD_AHEAD', 'OATHKEEP_CARD_BUFFERS_MIN_KB'
)) {
    Remove-Item "Env:$v" -ErrorAction SilentlyContinue
}

New-Item -ItemType Directory -Force -Path $Cache, $Data | Out-Null
$env:OATHKEEP_CACHE_DIR = $Cache
$env:OATHKEEP_DATA_DIR = $Data
$env:OATHKEEP_GAME_DIR = Split-Path -Parent (Resolve-Path -LiteralPath $Exe).Path

switch ($Profile) {
    'safe' {
        $env:OATHKEEP_SHADER_LOOP_LIMIT = '256'
    }
    'fast' {
        $env:OATHKEEP_SHADER_LOOP_LIMIT = '256'
        $env:OATHKEEP_LATE_WAIT = '3'
        $env:OATHKEEP_WIDE_BACK = '1'
        $env:OATHKEEP_WIDE_REFRESH = '1'
        $env:OATHKEEP_WIDE_MERGE = '1'
        $env:OATHKEEP_JOIN_COPIES = '1'
        $env:OATHKEEP_CARD_REHASH_RUNS = '1'
        $env:OATHKEEP_CARD_BUFFERS_MIN_MB = '1'
        $env:OATHKEEP_CARD_BUFFERS_MB = '1024'
        $env:OATHKEEP_TEXTURE_NO_WAIT = '1'
        $env:OATHKEEP_PAGE_WRITES = '1'
        $env:OATHKEEP_COMMITTED_QUERY = '1'
        $env:OATHKEEP_INDIRECT_WINDOW = '1'
        $env:OATHKEEP_INPLACE = '2'
        $env:OATHKEEP_INPLACE_MIN_MB = '1'
        $env:OATHKEEP_PIPELINE_WAIT_MS = '16'
    }
    'pareto' {
        $env:OATHKEEP_AMBIENT = '0.8'
        $env:OATHKEEP_AMBIENT_AIR = '1.5'
        $env:OATHKEEP_TEXTURE_NO_WAIT = '1'
        $env:OATHKEEP_RELAY = '2'
        $env:OATHKEEP_WIDE_BACK = '1'
        $env:OATHKEEP_CARD_REHASH_RUNS = '1'
        $env:OATHKEEP_JOIN_COPIES = '1'
        $env:OATHKEEP_INDIRECT_WINDOW = '1'
        $env:OATHKEEP_RELAY_COPY = '1'
        $env:OATHKEEP_CARD_STEPS = '2'
        $env:OATHKEEP_INDIRECT_BATCH = '2'
        $env:OATHKEEP_MAP_THREADS = '1'
        $env:OATHKEEP_CARD_AWAKE = '1'
        $env:OATHKEEP_SHOW_AFTER = '1'
        $env:OATHKEEP_THIN = '1'
        $env:OATHKEEP_JOIN_PARTS = '8'
        $env:OATHKEEP_UPLOAD_AHEAD = '1'
        $env:OATHKEEP_LEVELS_ON_DEMAND = '1'
        $env:OATHKEEP_CARD_BUFFERS_MIN_KB = '64'
    }
    'nvidia8gb' {
        $env:OATHKEEP_DISPLAY_MODE = '1920x1080'
        $env:OATHKEEP_AMBIENT = '0.8'
        $env:OATHKEEP_AMBIENT_AIR = '1.5'
        $env:OATHKEEP_TEXTURE_NO_WAIT = '1'
        $env:OATHKEEP_RELAY = '2'
        $env:OATHKEEP_WIDE_BACK = '1'
        $env:OATHKEEP_CARD_REHASH_RUNS = '1'
        $env:OATHKEEP_JOIN_COPIES = '1'
        $env:OATHKEEP_INDIRECT_WINDOW = '1'
        $env:OATHKEEP_RELAY_COPY = '1'
        $env:OATHKEEP_CARD_STEPS = '2'
        $env:OATHKEEP_INDIRECT_BATCH = '2'
        $env:OATHKEEP_MAP_THREADS = '1'
        $env:OATHKEEP_CARD_AWAKE = '1'
        $env:OATHKEEP_SHOW_AFTER = '1'
        $env:OATHKEEP_THIN = '1'
        $env:OATHKEEP_JOIN_PARTS = '8'
        $env:OATHKEEP_UPLOAD_AHEAD = '1'
        $env:OATHKEEP_LEVELS_ON_DEMAND = '1'
        $env:OATHKEEP_CARD_BUFFERS_MIN_KB = '64'
    }
}

if ($Dred) {
    $env:OATHKEEP_D3D12_DRED = '1'
}

$runArgs = @('run')
if ($Console -ne 'auto') {
    $runArgs += @('--console', $Console)
}
$runArgs += @($Exe, '--execute')

Write-Host "Launching Oathkeep ($Profile, console=$Console) -> $Exe"
& $oathkeepExe @runArgs
exit $LASTEXITCODE
