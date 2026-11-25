<#
.SYNOPSIS
  Downloads Microsoft's official DirectX 12 Agility SDK (Microsoft.Direct3D.D3D12)
  from NuGet into temporary storage and installs D3D12Core.dll next to Oathkeep's
  build outputs (target\release\D3D12 and target\diag\D3D12).

  Required on Windows 10 (build 19041+) for Shader Model 6.6 dynamic resource
  root signatures (flags 0xc01). Never commits DLLs into the repository.
#>
[CmdletBinding()]
param(
    [string]$Version = '1.620.2'
)

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$pkgUrl = "https://api.nuget.org/v3-flatcontainer/microsoft.direct3d.d3d12/$Version/microsoft.direct3d.d3d12.$Version.nupkg"
$work = Join-Path $env:TEMP "oathkeep-agility-$Version"
$zipPath = Join-Path $work "agility.zip"
$extractDir = Join-Path $work "extract"

New-Item -ItemType Directory -Force -Path $work | Out-Null
if (-not (Test-Path -LiteralPath $extractDir)) {
    Write-Host "Downloading Microsoft.Direct3D.D3D12 $Version from NuGet..."
    Invoke-WebRequest -Uri $pkgUrl -OutFile $zipPath -UseBasicParsing
    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
}

$binDir = Join-Path $extractDir 'build\native\bin\x64'
$coreDll = Join-Path $binDir 'D3D12Core.dll'
if (-not (Test-Path -LiteralPath $coreDll)) {
    throw "D3D12Core.dll not found inside NuGet package at $binDir"
}

$targets = @(
    (Join-Path $repo 'target\release\D3D12'),
    (Join-Path $repo 'target\diag\D3D12'),
    (Join-Path $repo 'target\debug\D3D12')
)

foreach ($dst in $targets) {
    New-Item -ItemType Directory -Force -Path $dst | Out-Null
    Copy-Item -LiteralPath $coreDll -Destination (Join-Path $dst 'D3D12Core.dll') -Force
    $layers = Join-Path $binDir 'd3d12SDKLayers.dll'
    if (Test-Path -LiteralPath $layers) {
        Copy-Item -LiteralPath $layers -Destination (Join-Path $dst 'd3d12SDKLayers.dll') -Force
    }
    Write-Host "Installed Agility SDK $Version -> $dst"
}
