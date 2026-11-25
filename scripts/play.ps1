<#
.SYNOPSIS
  Builds (when needed, capped at -j 4) and launches Oathkeep on a user-supplied
  Xbox Series X|S title executable. Keeps the title directory strictly read-only
  and stores all caches/saves outside the repository.

.PARAMETER Exe
  Path to your own console title executable (kept outside this repository).

.PARAMETER Profile
  Tuning preset:
  - safe      : upstream conservative defaults (best first-run stability)
  - fast      : late waits, wide back-end, card buffers, loop limit 256
  - nvidia8gb : fast + levels-on-demand VRAM budgeting for 8 GB GPUs

.PARAMETER Diag
  Use the diagnostics build (`--profile diag --features diagnostics`).

.PARAMETER Dred
  Enable D3D12 DRED (slows frames; use only to diagnose GPU hangs).

.PARAMETER Cache
  Override cache directory (default: %LOCALAPPDATA%\oathkeep\cache).

.PARAMETER Data
  Override saves/logs directory (default: %LOCALAPPDATA%\oathkeep\data).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [ValidateSet('safe', 'fast', 'nvidia8gb')][string]$Profile = 'safe',
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

if ($Diag) {
    Write-Host 'Building Oathkeep (diag, diagnostics, -j 4)...'
    & cargo build -j 4 --profile diag -p victory --features diagnostics --manifest-path $manifest
    $binDir = Join-Path $repo 'target\diag'
} else {
    Write-Host 'Building Oathkeep (release, -j 4)...'
    & cargo build -j 4 --release -p victory --manifest-path $manifest
    $binDir = Join-Path $repo 'target\release'
}
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit code $LASTEXITCODE)." }

$oathkeepExe = Join-Path $binDir 'oathkeep.exe'

# Windows 10 (build < 22000) needs DirectX 12 Agility SDK for SM 6.6 dynamic resources.
$osBuild = [Environment]::OSVersion.Version.Build
$agilityDll = Join-Path $binDir 'D3D12\D3D12Core.dll'
if ($osBuild -lt 22000 -and -not (Test-Path -LiteralPath $agilityDll)) {
    Write-Host "Windows 10 detected (build $osBuild) without D3D12Core.dll; running setup-agility-sdk.ps1..."
    & (Join-Path $PSScriptRoot 'setup-agility-sdk.ps1')
}

foreach ($v in @(
    'VICTORY_KEYS', 'VICTORY_CAPTURE', 'VICTORY_CAPTURE_FRAMES', 'VICTORY_DUMP_TARGETS',
    'VICTORY_LATE_WAIT', 'VICTORY_WIDE_BACK', 'VICTORY_RELAY', 'VICTORY_CARD_BUFFERS_MIN_MB',
    'VICTORY_CARD_BUFFERS_MB', 'VICTORY_SHADER_LOOP_LIMIT', 'VICTORY_LEVELS_ON_DEMAND',
    'VICTORY_D3D12_DRED'
)) {
    Remove-Item "Env:$v" -ErrorAction SilentlyContinue
}

New-Item -ItemType Directory -Force -Path $Cache, $Data | Out-Null
$env:VICTORY_CACHE_DIR = $Cache
$env:VICTORY_DATA_DIR = $Data
$env:VICTORY_GAME_DIR = Split-Path -Parent (Resolve-Path -LiteralPath $Exe).Path

switch ($Profile) {
    'safe' {
        $env:VICTORY_SHADER_LOOP_LIMIT = '256'
    }
    'fast' {
        $env:VICTORY_SHADER_LOOP_LIMIT = '256'
        $env:VICTORY_LATE_WAIT = '3'
        $env:VICTORY_WIDE_BACK = '1'
        $env:VICTORY_CARD_BUFFERS_MIN_MB = '1'
        $env:VICTORY_CARD_BUFFERS_MB = '1024'
    }
    'nvidia8gb' {
        $env:VICTORY_SHADER_LOOP_LIMIT = '256'
        $env:VICTORY_LATE_WAIT = '3'
        $env:VICTORY_WIDE_BACK = '1'
        $env:VICTORY_CARD_BUFFERS_MIN_MB = '1'
        $env:VICTORY_CARD_BUFFERS_MB = '1024'
        $env:VICTORY_LEVELS_ON_DEMAND = '1'
        $env:VICTORY_PIPELINE_WAIT_MS = '24'
    }
}

if ($Dred) {
    $env:VICTORY_D3D12_DRED = '1'
}

Write-Host "Launching Oathkeep ($Profile) -> $Exe"
& $oathkeepExe run $Exe --execute
exit $LASTEXITCODE
