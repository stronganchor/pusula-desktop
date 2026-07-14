[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $ExpectedVersion,
    [Parameter(Mandatory = $true)][string] $Repository,
    [Parameter(Mandatory = $true)][string] $ExpectedCommit,
    [Parameter(Mandatory = $true)][ValidateSet('true', 'false')][string] $Prerelease
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-SemVer.ps1')
$candidate = ConvertTo-StrictSemVer -Version $ExpectedVersion

if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw "Invalid GitHub repository name: $Repository"
}
if ($ExpectedCommit -notmatch '^[0-9a-fA-F]{40}$') {
    throw 'Expected release commit must be a full 40-character Git SHA.'
}
if ($Prerelease -eq 'false' -and $null -ne $candidate.Prerelease) {
    throw 'A stable GitHub release cannot use a prerelease SemVer suffix.'
}
if ($env:GITHUB_REF -and $env:GITHUB_REF -ne 'refs/heads/main') {
    throw "Release workflows must be dispatched from main, not $($env:GITHUB_REF)."
}

foreach ($command in 'git', 'gh') {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "Required release command was not found: $command"
    }
}

$head = (& git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $head -ne $ExpectedCommit) {
    throw "Checked-out commit $head does not match immutable workflow commit $ExpectedCommit."
}

& (Join-Path $PSScriptRoot 'Test-VersionConsistency.ps1') -ExpectedVersion $ExpectedVersion | Out-Host

$mainResponse = & gh api "repos/$Repository/git/ref/heads/main"
if ($LASTEXITCODE -ne 0) { throw 'Could not verify the current main branch through GitHub.' }
$mainCommit = [string](($mainResponse | ConvertFrom-Json).object.sha)
if ($mainCommit -ne $ExpectedCommit) {
    throw "main moved to $mainCommit after workflow dispatch; refusing to release $ExpectedCommit."
}

$matchingTagsResponse = & gh api "repos/$Repository/git/matching-refs/tags/v$ExpectedVersion"
if ($LASTEXITCODE -ne 0) { throw 'Could not verify existing Git tags through GitHub.' }
$matchingTags = @($matchingTagsResponse | ConvertFrom-Json)
if ($matchingTags.Count -ne 0) {
    throw "Tag v$ExpectedVersion already exists. Releases are immutable and cannot be overwritten."
}

$releaseRows = @(& gh api --paginate "repos/$Repository/releases?per_page=100" --jq '.[] | [.tag_name, .draft] | @tsv')
if ($LASTEXITCODE -ne 0) { throw 'Could not verify existing GitHub releases.' }

$highestPublished = $null
foreach ($row in $releaseRows) {
    if ([string]::IsNullOrWhiteSpace($row)) { continue }
    $columns = @($row -split "`t", 2)
    $tag = $columns[0]
    $draft = $columns.Count -gt 1 -and $columns[1] -eq 'true'
    if ($tag -eq "v$ExpectedVersion") {
        throw "GitHub release v$ExpectedVersion already exists. Inspect or remove the failed draft manually."
    }
    if ($draft -or -not $tag.StartsWith('v', [StringComparison]::Ordinal)) { continue }

    $publishedVersion = $tag.Substring(1)
    try {
        $null = ConvertTo-StrictSemVer -Version $publishedVersion
    }
    catch {
        continue
    }

    if ($null -eq $highestPublished -or
        (Compare-StrictSemVer -Left $publishedVersion -Right $highestPublished) -gt 0) {
        $highestPublished = $publishedVersion
    }
}

if ($null -ne $highestPublished -and
    (Compare-StrictSemVer -Left $ExpectedVersion -Right $highestPublished) -le 0) {
    throw "Release version $ExpectedVersion must be greater than published version $highestPublished."
}

Write-Output "Release identity verified: v$ExpectedVersion at $ExpectedCommit (prerelease=$Prerelease)"
