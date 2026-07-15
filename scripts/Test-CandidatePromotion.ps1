[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Version,
    [Parameter(Mandatory = $true)][string] $CandidateTag,
    [Parameter(Mandatory = $true)][string] $Repository,
    [Parameter(Mandatory = $true)][string] $WorkflowCommit,
    [Parameter(Mandatory = $true)][string] $AcceptanceEvidenceSha256,
    [Parameter(Mandatory = $true)][string] $AcceptanceEvidencePath,
    [Parameter(Mandatory = $true)][string] $Confirmation,
    [Parameter(Mandatory = $true)][string] $DownloadDirectory,
    [Parameter(Mandatory = $true)][string] $ExpectedWindowsPublisher,
    [Parameter(Mandatory = $true)][string] $ExpectedWindowsCertificateSha256,
    [Parameter(Mandatory = $true)][string] $ActionsToken,
    [Parameter(Mandatory = $true)][string] $MinisignPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-SemVer.ps1')
. (Join-Path $PSScriptRoot 'Release-RepositoryControls.ps1')
$candidate = ConvertTo-StrictSemVer -Version $Version
if ($null -ne $candidate.Prerelease) {
    throw 'A production promotion must use the final SemVer, not a prerelease suffix.'
}
if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw "Invalid GitHub repository name: $Repository"
}
if ($WorkflowCommit -cnotmatch '^[0-9a-f]{40}$') {
    throw 'Workflow commit must be a lowercase full 40-character Git SHA.'
}
$candidateTagPattern = '^v' + [regex]::Escape($Version) + '-candidate\.(?<commit>[0-9a-f]{40})$'
if ($CandidateTag -cnotmatch $candidateTagPattern) {
    throw "Candidate tag must exactly match v$Version-candidate.<lowercase full Git SHA>."
}
$candidateCommit = [string]$Matches.commit
if ($candidateCommit -cne $WorkflowCommit) {
    throw 'Promotion must be dispatched from the exact accepted candidate commit on main.'
}
if ($AcceptanceEvidenceSha256 -notmatch '^[0-9a-fA-F]{64}$') {
    throw 'Acceptance evidence must be identified by a SHA-256 value.'
}
if ($Confirmation -cne "PROMOTE v$Version") {
    throw "Confirmation must exactly equal: PROMOTE v$Version"
}
if ([string]::IsNullOrWhiteSpace($ExpectedWindowsPublisher)) {
    throw 'Expected Windows publisher must be configured for promotion.'
}
if ($ExpectedWindowsCertificateSha256 -cnotmatch '^[0-9a-fA-F]{64}$') {
    throw 'Expected Windows certificate SHA-256 must be configured for promotion.'
}
if ([string]::IsNullOrWhiteSpace($ActionsToken)) {
    throw 'An Actions-read token is required for promotion evidence verification.'
}
if ($env:GITHUB_REF -and $env:GITHUB_REF -cne 'refs/heads/main') {
    throw 'Candidate promotion must be dispatched from main.'
}

$head = (& git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $head -cne $WorkflowCommit) {
    throw 'Promotion workflow checkout does not match its immutable event commit.'
}
& (Join-Path $PSScriptRoot 'Test-VersionConsistency.ps1') -ExpectedVersion $Version | Out-Host

$null = Assert-PusulaReleaseRepositoryControls -Repository $Repository
$releaseApiHeader = @('-H', 'X-GitHub-Api-Version: 2026-03-10')

$stableTag = "v$Version"
$matchingTagsResponse = & gh api "repos/$Repository/git/matching-refs/tags/$stableTag"
if ($LASTEXITCODE -ne 0) { throw 'Could not verify existing stable Git tags through GitHub.' }
$parsedMatchingTags = $matchingTagsResponse | ConvertFrom-Json
$matchingTags = @($parsedMatchingTags)
$exactStableRefs = @($matchingTags | Where-Object { [string]$_.ref -ceq "refs/tags/$stableTag" })
if ($exactStableRefs.Count -gt 1) { throw "Stable tag lookup is ambiguous: $stableTag" }
if ($exactStableRefs.Count -eq 1 -and
    ([string]$exactStableRefs[0].object.type -cne 'commit' -or
     [string]$exactStableRefs[0].object.sha -cne $candidateCommit)) {
    throw "Existing stable tag is not pinned to the accepted candidate commit: $stableTag"
}

$stableReleaseIds = @(& gh api @releaseApiHeader --paginate "repos/$Repository/releases?per_page=100" --jq ".[] | select(.tag_name == `"$stableTag`") | .id")
if ($LASTEXITCODE -ne 0) { throw 'Could not verify existing stable GitHub releases.' }
$stableReleaseIds = @($stableReleaseIds | ForEach-Object {
        $id = 0L
        if (-not [long]::TryParse(([string]$_).Trim(), [ref]$id) -or $id -le 0) {
            throw 'Stable release lookup returned an invalid numeric ID.'
        }
        $id
    })
if ($stableReleaseIds.Count -gt 1) { throw "Stable release lookup is ambiguous: $stableTag" }
if ($stableReleaseIds.Count -eq 1) {
    $existingStableJson = @(& gh api @releaseApiHeader "repos/$Repository/releases/$($stableReleaseIds[0])")
    if ($LASTEXITCODE -ne 0) { throw 'Could not inspect the existing stable release by numeric ID.' }
    $existingStable = ($existingStableJson -join "`n") | ConvertFrom-Json
    if (-not [bool]$existingStable.draft -or [bool]$existingStable.prerelease -or
        [bool]$existingStable.immutable -or
        [string]$existingStable.tag_name -cne $stableTag -or
        [string]$existingStable.target_commitish -cne $candidateCommit -or
        [long]$existingStable.id -ne [long]$stableReleaseIds[0]) {
        throw "Existing stable release is not the exact resumable draft: $stableTag"
    }
    if ($exactStableRefs.Count -ne 1) {
        throw 'A resumable stable draft must retain its exact protected tag.'
    }
}

$releaseJson = & gh release view $CandidateTag --repo $Repository --json tagName,isDraft,isImmutable,isPrerelease,targetCommitish,assets
if ($LASTEXITCODE -ne 0) { throw "Candidate release does not exist: $CandidateTag" }
$release = $releaseJson | ConvertFrom-Json
if ([string]$release.tagName -cne $CandidateTag -or $release.isDraft -or -not $release.isPrerelease) {
    throw 'Only the exact published prerelease candidate can be promoted.'
}
if (-not [bool]$release.isImmutable) {
    throw 'Only an immutable candidate release can be promoted.'
}
if ([string]$release.targetCommitish -cne $candidateCommit) {
    throw 'Candidate release target does not match the commit embedded in its immutable tag.'
}

$tagRefJson = & gh api "repos/$Repository/git/ref/tags/$CandidateTag"
if ($LASTEXITCODE -ne 0) { throw 'Could not verify the immutable candidate Git tag.' }
$tagRef = $tagRefJson | ConvertFrom-Json
if ([string]$tagRef.object.type -cne 'commit' -or [string]$tagRef.object.sha -cne $candidateCommit) {
    throw 'Candidate Git tag is not a lightweight tag pinned to the accepted commit.'
}
& git merge-base --is-ancestor $candidateCommit $head
if ($LASTEXITCODE -ne 0) { throw 'Candidate release commit is not an ancestor of current main.' }

if (Test-Path -LiteralPath $DownloadDirectory) {
    $existing = @(Get-ChildItem -LiteralPath $DownloadDirectory -Force -ErrorAction Stop)
    if ($existing.Count -ne 0) { throw 'Candidate download directory must be empty.' }
}
else {
    [IO.Directory]::CreateDirectory($DownloadDirectory) | Out-Null
}

gh release download $CandidateTag --repo $Repository --dir $DownloadDirectory
if ($LASTEXITCODE -ne 0) { throw 'Could not download candidate release assets for revalidation.' }
& (Join-Path $PSScriptRoot 'Test-ReleaseAssets.ps1') `
    -Directory $DownloadDirectory `
    -Version $Version `
    -ReleaseTag $CandidateTag `
    -Repository $Repository | Out-Host

$minisign = (Resolve-Path -LiteralPath $MinisignPath -ErrorAction Stop).Path
$updaterName = "Pusula_${Version}_x64-setup.exe"
& (Join-Path $PSScriptRoot 'Test-TauriUpdaterSignature.ps1') `
    -ArtifactPath (Join-Path $DownloadDirectory $updaterName) `
    -SignaturePath (Join-Path $DownloadDirectory "$updaterName.sig") `
    -TauriConfigPath (Join-Path (Split-Path $PSScriptRoot -Parent) 'src-tauri\tauri.conf.json') `
    -MinisignPath $minisign | Out-Host

$offlineInstaller = Get-Item -LiteralPath (Join-Path $DownloadDirectory "Pusula_${Version}_x64_offline-setup.exe")
$leanInstaller = Get-Item -LiteralPath (Join-Path $DownloadDirectory "Pusula_${Version}_x64-setup.exe")
foreach ($executable in @($offlineInstaller, $leanInstaller)) {
    $signature = Get-AuthenticodeSignature -LiteralPath $executable.FullName
    if ($signature.Status -ne 'Valid') {
        throw "Invalid Authenticode signature: $($executable.Name) [$($signature.Status)]"
    }
    if (-not $signature.TimeStamperCertificate) {
        throw "Missing Authenticode timestamp: $($executable.Name)"
    }
    $publisher = $signature.SignerCertificate.GetNameInfo(
        [Security.Cryptography.X509Certificates.X509NameType]::SimpleName,
        $false
    )
    if (-not [string]::Equals($publisher, $ExpectedWindowsPublisher, [StringComparison]::Ordinal)) {
        throw "Unexpected Authenticode publisher for $($executable.Name): $publisher"
    }
    $certificateHash = ([BitConverter]::ToString(
            [Security.Cryptography.SHA256]::Create().ComputeHash($signature.SignerCertificate.RawData)
        )).Replace('-', '').ToLowerInvariant()
    if ($certificateHash -cne $ExpectedWindowsCertificateSha256.ToLowerInvariant()) {
        throw "Unexpected Authenticode certificate for $($executable.Name): $certificateHash"
    }
}

& (Join-Path $PSScriptRoot 'Test-ReleaseAcceptanceEvidence.ps1') `
    -EvidencePath $AcceptanceEvidencePath `
    -ExpectedSha256 $AcceptanceEvidenceSha256 `
    -CandidateAssetDirectory $DownloadDirectory `
    -Repository $Repository `
    -Version $Version `
    -CandidateTag $CandidateTag `
    -CandidateCommit $candidateCommit `
    -ExpectedWindowsPublisher $ExpectedWindowsPublisher `
    -ExpectedWindowsCertificateSha256 $ExpectedWindowsCertificateSha256 `
    -ActionsToken $ActionsToken | Out-Host

Write-Output "Immutable candidate $CandidateTag is eligible for stable publication; acceptance evidence SHA-256: $($AcceptanceEvidenceSha256.ToLowerInvariant())"
