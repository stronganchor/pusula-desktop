param(
    [string]$ExpectedVersion = ''
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path $PSScriptRoot -Parent
$package = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'package.json') | ConvertFrom-Json
$tauri = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'src-tauri\tauri.conf.json') | ConvertFrom-Json
$cargoText = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'src-tauri\Cargo.toml')
$cargoMatch = [regex]::Match($cargoText, '(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"')
if (-not $cargoMatch.Success) { throw 'Could not read the Cargo package version.' }

$versions = [ordered]@{
    package_json = [string]$package.version
    tauri_config = [string]$tauri.version
    cargo_toml = [string]$cargoMatch.Groups[1].Value
}

$unique = @($versions.Values | Select-Object -Unique)
if ($unique.Count -ne 1) {
    throw "Version metadata differs: $($versions | ConvertTo-Json -Compress)"
}
if ($ExpectedVersion -and $unique[0] -ne $ExpectedVersion) {
    throw "Expected version $ExpectedVersion, found $($unique[0])."
}

Write-Output "Version metadata is consistent: $($unique[0])"

