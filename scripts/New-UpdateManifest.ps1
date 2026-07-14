param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$ReleaseTag,
    [Parameter(Mandatory = $true)][string]$SignaturePath,
    [Parameter(Mandatory = $true)][string]$ArtifactName,
    [Parameter(Mandatory = $true)][string]$Repository,
    [Parameter(Mandatory = $true)][string]$OutputPath
)

$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-SemVer.ps1')
$parsed = ConvertTo-StrictSemVer -Version $Version
if ($null -ne $parsed.Prerelease) {
    throw 'The update manifest must use the final SemVer without a prerelease suffix.'
}
$stableTag = "v$Version"
$candidateTagPattern = '^v' + [regex]::Escape($Version) + '-candidate\.[0-9a-f]{40}$'
if ($ReleaseTag -cne $stableTag -and $ReleaseTag -cnotmatch $candidateTagPattern) {
    throw "Release tag must be $stableTag or its immutable candidate tag."
}
if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw "Invalid GitHub repository name: $Repository"
}
$expectedArtifactName = "Pusula_${Version}_x64.nsis.zip"
if ($ArtifactName -cne $expectedArtifactName) {
    throw "Updater artifact name must exactly equal $expectedArtifactName."
}

$signature = (Get-Content -Raw -LiteralPath $SignaturePath).Trim()
if ([string]::IsNullOrWhiteSpace($signature)) {
    throw 'Updater signature is empty.'
}

$downloadUrl = "https://github.com/$Repository/releases/download/$ReleaseTag/$ArtifactName"
$manifest = [ordered]@{
    version = $Version
    notes = "Pusula $Version"
    pub_date = [DateTimeOffset]::UtcNow.ToString('o')
    platforms = [ordered]@{
        'windows-x86_64' = [ordered]@{
            signature = $signature
            url = $downloadUrl
        }
    }
}

$manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $OutputPath -Encoding UTF8
