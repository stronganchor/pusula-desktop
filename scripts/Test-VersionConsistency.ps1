param(
    [string]$ExpectedVersion = ''
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'Release-SemVer.ps1')
$package = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'package.json') | ConvertFrom-Json
$packageLockText = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'package-lock.json')
$packageLockMatch = [regex]::Match(
    $packageLockText,
    '^\s*\{\s*"name"\s*:\s*"[^"]+"\s*,\s*"version"\s*:\s*"([^"]+)"'
)
$packageLockRootMatch = [regex]::Match(
    $packageLockText,
    '(?ms)"packages"\s*:\s*\{\s*""\s*:\s*\{.*?^\s*"version"\s*:\s*"([^"]+)"'
)
if (-not $packageLockMatch.Success -or -not $packageLockRootMatch.Success) {
    throw 'Could not read the package-lock.json versions.'
}
$tauri = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'src-tauri\tauri.conf.json') | ConvertFrom-Json
$cargoText = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'src-tauri\Cargo.toml')
$cargoMatch = [regex]::Match($cargoText, '(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"')
if (-not $cargoMatch.Success) { throw 'Could not read the Cargo package version.' }
$cargoLockText = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'src-tauri\Cargo.lock')
$cargoLockMatch = [regex]::Match(
    $cargoLockText,
    '(?ms)^\[\[package\]\]\s*name\s*=\s*"pusula-desktop"\s*version\s*=\s*"([^"]+)"'
)
if (-not $cargoLockMatch.Success) { throw 'Could not read the Pusula Cargo.lock version.' }

$versions = [ordered]@{
    package_json = [string]$package.version
    package_lock = [string]$packageLockMatch.Groups[1].Value
    package_lock_root = [string]$packageLockRootMatch.Groups[1].Value
    tauri_config = [string]$tauri.version
    cargo_toml = [string]$cargoMatch.Groups[1].Value
    cargo_lock = [string]$cargoLockMatch.Groups[1].Value
}

$unique = @($versions.Values | Select-Object -Unique)
if ($unique.Count -ne 1) {
    throw "Version metadata differs: $($versions | ConvertTo-Json -Compress)"
}
if ($ExpectedVersion -and $unique[0] -ne $ExpectedVersion) {
    throw "Expected version $ExpectedVersion, found $($unique[0])."
}
$null = ConvertTo-StrictSemVer -Version $unique[0]
if ($ExpectedVersion) { $null = ConvertTo-StrictSemVer -Version $ExpectedVersion }

Write-Output "Version metadata is consistent: $($unique[0])"
