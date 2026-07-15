[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $ExpectedVersion,
    [Parameter(Mandatory = $true)][string] $Repository,
    [Parameter(Mandatory = $true)][string] $ExpectedCommit,
    [Parameter(Mandatory = $true)][string] $CandidateTag,
    [Parameter(Mandatory = $true)][ValidateSet('true', 'false')][string] $BuildInitialAcceptanceBaseline,
    [Parameter(Mandatory = $true)][string] $AcceptanceBaselineVersion,
    [switch] $AllowExactCandidateDraftResume,
    [switch] $RequireRepositoryImmutability
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-SemVer.ps1')
. (Join-Path $PSScriptRoot 'Release-RepositoryControls.ps1')
$candidate = ConvertTo-StrictSemVer -Version $ExpectedVersion

if ($null -ne $candidate.Prerelease) {
    throw 'An acceptance candidate must use the final SemVer without a prerelease suffix; GitHub prerelease state carries the candidate status.'
}
if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw "Invalid GitHub repository name: $Repository"
}
if ($ExpectedCommit -notmatch '^[0-9a-fA-F]{40}$') {
    throw 'Expected release commit must be a full 40-character Git SHA.'
}
$normalizedCommit = $ExpectedCommit.ToLowerInvariant()
$expectedCandidateTag = Get-ReleaseCandidateTag -Version $ExpectedVersion -Commit $normalizedCommit
if ($CandidateTag -cne $expectedCandidateTag) {
    throw "Candidate tag must exactly equal $expectedCandidateTag."
}
if ($env:GITHUB_REF -and $env:GITHUB_REF -cne 'refs/heads/main') {
    throw "Release workflows must be dispatched from main, not $($env:GITHUB_REF)."
}

foreach ($command in 'git', 'gh') {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "Required release command was not found: $command"
    }
}

$head = (& git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $head -cne $ExpectedCommit) {
    throw "Checked-out commit $head does not match immutable workflow commit $ExpectedCommit."
}

& (Join-Path $PSScriptRoot 'Test-VersionConsistency.ps1') -ExpectedVersion $ExpectedVersion | Out-Host

$mainResponse = & gh api "repos/$Repository/git/ref/heads/main"
if ($LASTEXITCODE -ne 0) { throw 'Could not verify the current main branch through GitHub.' }
$mainCommit = [string](($mainResponse | ConvertFrom-Json).object.sha)
if ($mainCommit -cne $ExpectedCommit) {
    throw "main moved to $mainCommit after workflow dispatch; refusing to release $ExpectedCommit."
}

if ($RequireRepositoryImmutability) {
    $null = Assert-PusulaReleaseRepositoryControls -Repository $Repository
}

$matchingTagsResponse = & gh api "repos/$Repository/git/matching-refs/tags/v$ExpectedVersion"
if ($LASTEXITCODE -ne 0) { throw 'Could not verify existing Git tags through GitHub.' }
$parsedMatchingTags = $matchingTagsResponse | ConvertFrom-Json
$matchingTags = @($parsedMatchingTags)
$stableRef = "refs/tags/v$ExpectedVersion"
$candidateRefPrefix = "refs/tags/v$ExpectedVersion-candidate."
$stableRefs = @($matchingTags | Where-Object { [string]$_.ref -ceq $stableRef })
$candidateRefs = @($matchingTags | Where-Object {
        ([string]$_.ref).StartsWith($candidateRefPrefix, [StringComparison]::Ordinal)
    })
$exactCandidateRefs = @($candidateRefs | Where-Object { [string]$_.ref -ceq "refs/tags/$CandidateTag" })
if (-not $AllowExactCandidateDraftResume) {
    if ($stableRefs.Count -ne 0 -or $candidateRefs.Count -ne 0) {
        throw "A stable or candidate tag already exists for version $ExpectedVersion. Use a strictly greater version."
    }
}
elseif ($stableRefs.Count -ne 0 -or $candidateRefs.Count -ne $exactCandidateRefs.Count -or
    $exactCandidateRefs.Count -gt 1 -or
    ($exactCandidateRefs.Count -eq 1 -and
     ([string]$exactCandidateRefs[0].object.type -cne 'commit' -or
      [string]$exactCandidateRefs[0].object.sha -cne $normalizedCommit))) {
    throw 'Candidate draft resume requires only the exact lightweight candidate tag at the workflow commit.'
}

$releaseRows = @(& gh api --paginate "repos/$Repository/releases?per_page=100" --jq '.[] | [.tag_name, .draft, .prerelease] | @tsv')
if ($LASTEXITCODE -ne 0) { throw 'Could not verify existing GitHub releases.' }

$highestPublished = $null
$resumableCandidateReleaseCount = 0
foreach ($row in $releaseRows) {
    if ([string]::IsNullOrWhiteSpace($row)) { continue }
    $columns = @($row -split "`t", 3)
    $tag = $columns[0]
    $draft = $columns.Count -gt 1 -and $columns[1] -eq 'true'
    $prerelease = $columns.Count -gt 2 -and $columns[2] -eq 'true'
    if ($tag -ceq "v$ExpectedVersion") {
        throw "A stable or candidate GitHub release already exists for version $ExpectedVersion. Inspect any draft or use a strictly greater version."
    }
    if ($tag.StartsWith("v$ExpectedVersion-candidate.", [StringComparison]::Ordinal)) {
        if ($AllowExactCandidateDraftResume -and $tag -ceq $CandidateTag -and $draft -and $prerelease) {
            $resumableCandidateReleaseCount += 1
            continue
        }
        throw "A stable or candidate GitHub release already exists for version $ExpectedVersion. Inspect any draft or use a strictly greater version."
    }
    if ($draft -or $prerelease -or -not $tag.StartsWith('v', [StringComparison]::Ordinal)) { continue }

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
if ($resumableCandidateReleaseCount -gt 1 -or
    ($resumableCandidateReleaseCount -eq 1 -and $exactCandidateRefs.Count -ne 1)) {
    throw 'Candidate draft resume requires exactly one private prerelease draft and its exact protected tag.'
}

if ($null -eq $highestPublished) {
    if ($BuildInitialAcceptanceBaseline -cne 'true') {
        throw 'The first stable release requires build_initial_acceptance_baseline=true.'
    }
    if ($AcceptanceBaselineVersion -cne '0.0.9') {
        throw 'The first stable release requires the exact 0.0.9 acceptance baseline.'
    }
    if ((Compare-StrictSemVer -Left $AcceptanceBaselineVersion -Right $ExpectedVersion) -ge 0) {
        throw 'The initial 0.0.9 acceptance baseline must be lower than the release version.'
    }
}
elseif ($BuildInitialAcceptanceBaseline -cne 'false') {
    throw 'A synthetic acceptance baseline is allowed only before the first stable release.'
}

Write-Output "Release candidate identity verified: $CandidateTag at $ExpectedCommit"
