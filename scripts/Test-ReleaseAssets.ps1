[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Directory,
    [Parameter(Mandatory = $true)][string] $Version,
    [Parameter(Mandatory = $true)][string] $ReleaseTag,
    [Parameter(Mandatory = $true)][string] $Repository
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-SemVer.ps1')
$parsedVersion = ConvertTo-StrictSemVer -Version $Version
if ($null -ne $parsedVersion.Prerelease) {
    throw 'Release assets must use the final SemVer without a prerelease suffix.'
}
if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw "Invalid GitHub repository name: $Repository"
}
$stableTag = "v$Version"
$candidateTagPattern = '^v' + [regex]::Escape($Version) + '-candidate\.[0-9a-f]{40}$'
if ($ReleaseTag -cne $stableTag -and $ReleaseTag -cnotmatch $candidateTagPattern) {
    throw "Release tag must be $stableTag or its immutable candidate tag."
}

$assetDirectory = (Resolve-Path -LiteralPath $Directory -ErrorAction Stop).Path
$updaterName = "Pusula_${Version}_x64.nsis.zip"
$expectedNames = @(
    "Pusula_${Version}_x64_offline-setup.exe",
    "Pusula_${Version}_x64-setup.exe",
    $updaterName,
    "$updaterName.sig",
    'latest.json',
    'SHA256SUMS.txt'
) | Sort-Object
$actualFiles = @(Get-ChildItem -LiteralPath $assetDirectory -File -Force)
$actualNames = @($actualFiles.Name | Sort-Object)
if (($actualNames -join "`n") -cne ($expectedNames -join "`n")) {
    throw "Release asset allowlist mismatch. Expected $($expectedNames -join ', '); found $($actualNames -join ', ')."
}
if (@(Get-ChildItem -LiteralPath $assetDirectory -Directory -Force).Count -ne 0) {
    throw 'Release asset directory must not contain subdirectories.'
}
foreach ($file in $actualFiles) {
    if ($file.Length -le 0) { throw "Release asset is empty: $($file.Name)" }
}

$manifest = Get-Content -Raw -LiteralPath (Join-Path $assetDirectory 'latest.json') | ConvertFrom-Json
if ([string]$manifest.version -cne $Version) { throw 'Update manifest version does not match the release.' }
$platformNames = @($manifest.platforms.PSObject.Properties.Name)
if ($platformNames.Count -ne 1 -or $platformNames[0] -cne 'windows-x86_64') {
    throw 'Update manifest must contain exactly the case-sensitive windows-x86_64 platform key.'
}
$platform = $manifest.platforms.'windows-x86_64'
if ($null -eq $platform -or [string]::IsNullOrWhiteSpace([string]$platform.signature)) {
    throw 'Update manifest is missing the windows-x86_64 signature.'
}
$expectedUrl = "https://github.com/$Repository/releases/download/$ReleaseTag/$updaterName"
if ([string]$platform.url -cne $expectedUrl) { throw 'Update manifest URL does not match the immutable release asset.' }
$signature = (Get-Content -Raw -LiteralPath (Join-Path $assetDirectory "$updaterName.sig")).Trim()
if ([string]$platform.signature -cne $signature) { throw 'Update manifest signature differs from the .sig asset.' }

$hashPath = Join-Path $assetDirectory 'SHA256SUMS.txt'
$hashRows = @(Get-Content -LiteralPath $hashPath | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
$hashedNames = New-Object System.Collections.Generic.HashSet[string]([StringComparer]::Ordinal)
foreach ($row in $hashRows) {
    if ($row -cnotmatch '^(?<hash>[0-9a-f]{64})  (?<name>[^\\/]+)$') {
        throw "Invalid SHA256SUMS.txt row: $row"
    }
    $name = $Matches.name
    if ($name -ceq 'SHA256SUMS.txt' -or -not $hashedNames.Add($name)) {
        throw "Invalid duplicate or self-referential hash entry: $name"
    }
    $path = Join-Path $assetDirectory $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Hash entry has no asset: $name" }
    $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
    if ($actualHash -cne $Matches.hash) { throw "SHA-256 mismatch for release asset: $name" }
}
$expectedHashedNames = @($expectedNames | Where-Object { $_ -cne 'SHA256SUMS.txt' } | Sort-Object)
if ((@($hashedNames) | Sort-Object) -join "`n" -cne ($expectedHashedNames -join "`n")) {
    throw 'SHA256SUMS.txt does not cover the exact release asset allowlist.'
}

Write-Output "Release asset allowlist and hashes verified for Pusula $Version."
