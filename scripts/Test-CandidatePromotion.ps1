[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Version,
    [Parameter(Mandatory = $true)][string] $Repository,
    [Parameter(Mandatory = $true)][string] $WorkflowCommit,
    [Parameter(Mandatory = $true)][string] $AcceptanceEvidenceSha256,
    [Parameter(Mandatory = $true)][string] $Confirmation,
    [Parameter(Mandatory = $true)][string] $DownloadDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-SemVer.ps1')
$candidate = ConvertTo-StrictSemVer -Version $Version
if ($null -ne $candidate.Prerelease) {
    throw 'A production promotion must use the final SemVer, not a prerelease suffix.'
}
if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw "Invalid GitHub repository name: $Repository"
}
if ($WorkflowCommit -notmatch '^[0-9a-fA-F]{40}$') {
    throw 'Workflow commit must be a full 40-character Git SHA.'
}
if ($AcceptanceEvidenceSha256 -notmatch '^[0-9a-fA-F]{64}$') {
    throw 'Acceptance evidence must be identified by a SHA-256 value.'
}
if ($Confirmation -cne "PROMOTE v$Version") {
    throw "Confirmation must exactly equal: PROMOTE v$Version"
}
if ($env:GITHUB_REF -and $env:GITHUB_REF -ne 'refs/heads/main') {
    throw 'Candidate promotion must be dispatched from main.'
}

$head = (& git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $head -ne $WorkflowCommit) {
    throw 'Promotion workflow checkout does not match its immutable event commit.'
}
& (Join-Path $PSScriptRoot 'Test-VersionConsistency.ps1') -ExpectedVersion $Version | Out-Host

$tag = "v$Version"
$releaseJson = & gh release view $tag --repo $Repository --json tagName,isDraft,isPrerelease,targetCommitish,assets
if ($LASTEXITCODE -ne 0) { throw "Candidate release does not exist: $tag" }
$release = $releaseJson | ConvertFrom-Json
if ([string]$release.tagName -ne $tag -or $release.isDraft -or -not $release.isPrerelease) {
    throw 'Only a published prerelease candidate can be promoted.'
}
if ([string]$release.targetCommitish -notmatch '^[0-9a-fA-F]{40}$') {
    throw 'Candidate release is not pinned to an immutable commit SHA.'
}
& git merge-base --is-ancestor ([string]$release.targetCommitish) $head
if ($LASTEXITCODE -ne 0) { throw 'Candidate release commit is not an ancestor of current main.' }

if (Test-Path -LiteralPath $DownloadDirectory) {
    $existing = @(Get-ChildItem -LiteralPath $DownloadDirectory -Force -ErrorAction Stop)
    if ($existing.Count -ne 0) { throw 'Candidate download directory must be empty.' }
}
else {
    [IO.Directory]::CreateDirectory($DownloadDirectory) | Out-Null
}

gh release download $tag --repo $Repository --dir $DownloadDirectory
if ($LASTEXITCODE -ne 0) { throw 'Could not download candidate release assets for revalidation.' }
& (Join-Path $PSScriptRoot 'Test-ReleaseAssets.ps1') `
    -Directory $DownloadDirectory `
    -Version $Version `
    -Repository $Repository | Out-Host

Write-Output "Candidate v$Version is eligible for promotion; acceptance evidence SHA-256: $($AcceptanceEvidenceSha256.ToLowerInvariant())"
