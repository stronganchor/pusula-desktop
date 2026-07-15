[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('Candidate', 'Stable')][string] $Mode,
    [Parameter(Mandatory = $true)][string] $Repository,
    [Parameter(Mandatory = $true)][string] $Tag,
    [Parameter(Mandatory = $true)][string] $ExpectedCommit,
    [Parameter(Mandatory = $true)][string] $ExpectedVersion,
    [Parameter(Mandatory = $true)][string] $AssetDirectory,
    [string] $BuildInitialAcceptanceBaseline = '',
    [string] $AcceptanceBaselineVersion = '',
    [string] $CandidateTag = '',
    [string] $AcceptanceEvidenceAssetName = '',
    [string] $AcceptanceEvidenceSha256 = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-RepositoryControls.ps1')

if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' -or
    $ExpectedVersion -cnotmatch '^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$' -or
    $ExpectedCommit -cnotmatch '^[0-9a-f]{40}$') {
    throw 'Release publication identity is invalid.'
}
if (($Mode -ceq 'Candidate' -and $Tag -cne "v$ExpectedVersion-candidate.$ExpectedCommit") -or
    ($Mode -ceq 'Stable' -and
     ($Tag -cne "v$ExpectedVersion" -or $CandidateTag -cne "v$ExpectedVersion-candidate.$ExpectedCommit"))) {
    throw 'Release publication tag identity is not deterministic for the expected version and commit.'
}
if ([string]::IsNullOrWhiteSpace($env:GH_TOKEN)) {
    throw 'Release publication requires the job-scoped contents-write token in GH_TOKEN.'
}
if ([string]::IsNullOrWhiteSpace($env:RELEASE_ADMIN_READ_TOKEN)) {
    throw 'Release publication requires the protected read-only administration token.'
}
$writeToken = $env:GH_TOKEN
$adminToken = $env:RELEASE_ADMIN_READ_TOKEN
$releaseApiHeader = @('-H', 'X-GitHub-Api-Version: 2026-03-10')

function Get-ExactReleaseByTag {
    param([string] $ExpectedTag)
    $rows = @(& gh api @releaseApiHeader --paginate "repos/$Repository/releases?per_page=100" --jq ".[] | select(.tag_name == `"$ExpectedTag`") | .id")
    if ($LASTEXITCODE -ne 0) { throw "Could not enumerate releases while finding $ExpectedTag." }
    $ids = @($rows | ForEach-Object {
            $id = 0L
            if (-not [long]::TryParse(([string]$_).Trim(), [ref]$id) -or $id -le 0) {
                throw "Release lookup for $ExpectedTag returned an invalid numeric ID."
            }
            $id
        })
    if ($ids.Count -ne 1) { throw "Expected exactly one release for tag $ExpectedTag." }
    $releaseRows = @(& gh api @releaseApiHeader "repos/$Repository/releases/$($ids[0])")
    if ($LASTEXITCODE -ne 0) { throw "Could not read release $ExpectedTag by numeric ID." }
    $release = ($releaseRows -join "`n") | ConvertFrom-Json
    if ([long]$release.id -ne [long]$ids[0] -or [string]$release.tag_name -cne $ExpectedTag) {
        throw "Numeric release identity differs from tag $ExpectedTag."
    }
    return $release
}

function Assert-ExactMainCheckoutAndTag {
    $mainRows = @(& gh api "repos/$Repository/git/ref/heads/main")
    if ($LASTEXITCODE -ne 0 -or [string](($mainRows -join "`n" | ConvertFrom-Json).object.sha) -cne $ExpectedCommit) {
        throw 'main is not the exact release commit immediately before publication.'
    }
    $head = (& git rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or $head -cne $ExpectedCommit) {
        throw 'Publication checkout is not the exact release commit.'
    }
    $tagRows = @(& gh api "repos/$Repository/git/ref/tags/$Tag")
    if ($LASTEXITCODE -ne 0) { throw 'Could not verify the protected release tag immediately before publication.' }
    $tagRef = ($tagRows -join "`n") | ConvertFrom-Json
    if ([string]$tagRef.ref -cne "refs/tags/$Tag" -or
        [string]$tagRef.object.type -cne 'commit' -or
        [string]$tagRef.object.sha -cne $ExpectedCommit) {
        throw 'Protected release tag is not a lightweight ref at the exact release commit.'
    }
}

function Assert-CandidateBaselineHistory {
    if ($Mode -cne 'Candidate') { return }
    $stableTags = @(& gh api @releaseApiHeader --paginate "repos/$Repository/releases?per_page=100" `
        --jq '.[] | select(.draft == false and .prerelease == false) | .tag_name')
    if ($LASTEXITCODE -ne 0) { throw 'Could not recheck stable-release history before candidate publication.' }
    $stableSemVerTags = @($stableTags | Where-Object {
            [string]$_ -cmatch '^v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$'
        })
    if ($stableSemVerTags.Count -eq 0) {
        if ($BuildInitialAcceptanceBaseline -cne 'true' -or $AcceptanceBaselineVersion -cne '0.0.9') {
            throw 'Final initial-candidate publication requires build_initial_acceptance_baseline=true and baseline 0.0.9.'
        }
    }
    elseif ($BuildInitialAcceptanceBaseline -cne 'false') {
        throw 'Final candidate publication forbids a synthetic baseline after a stable release exists.'
    }
}

function Get-LocalReleaseAssets {
    param([string] $Directory)
    $resolved = (Resolve-Path -LiteralPath $Directory -ErrorAction Stop).Path
    if (@(Get-ChildItem -LiteralPath $resolved -Directory -Force).Count -ne 0) {
        throw 'Release asset directory must not contain subdirectories.'
    }
    return @(Get-ChildItem -LiteralPath $resolved -File -Force | ForEach-Object {
            if ($_.Length -le 0) { throw "Release asset is empty: $($_.Name)" }
            [pscustomobject][ordered]@{
                name = $_.Name
                size = [long]$_.Length
                digest = "sha256:$((Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant())"
                path = $_.FullName
            }
        } | Sort-Object name)
}

function Get-VerifiedRemoteReleaseAssets {
    param($Release)
    $rows = @()
    $seenIds = New-Object 'System.Collections.Generic.HashSet[long]'
    $seenNames = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
    foreach ($summary in @($Release.assets)) {
        $assetId = [long]$summary.id
        if ($assetId -le 0 -or -not $seenIds.Add($assetId)) { throw 'Draft release has an invalid or duplicate asset ID.' }
        $assetJson = @(& gh api @releaseApiHeader "repos/$Repository/releases/assets/$assetId")
        if ($LASTEXITCODE -ne 0) { throw "Could not read draft asset ID $assetId." }
        $asset = ($assetJson -join "`n") | ConvertFrom-Json
        $name = [string]$asset.name
        if ([long]$asset.id -ne $assetId -or -not $seenNames.Add($name) -or [string]$asset.state -cne 'uploaded') {
            throw "Draft asset ID $assetId changed identity or is not fully uploaded."
        }
        $digest = ([string]$asset.digest).ToLowerInvariant()
        if ($digest -cnotmatch '^sha256:[0-9a-f]{64}$') { throw "Draft asset $name has no GitHub SHA-256 digest." }
        if ([string]$summary.name -cne $name -or [long]$summary.size -ne [long]$asset.size -or
            ([string]$summary.digest).ToLowerInvariant() -cne $digest) {
            throw "Draft asset ID $assetId differs between release and numeric-asset readback."
        }
        $rows += [pscustomobject][ordered]@{
            id = $assetId
            name = $name
            size = [long]$asset.size
            digest = $digest
        }
    }
    return @($rows | Sort-Object name)
}

function Assert-AssetMapsEqual {
    param($Local, $Remote, [string] $Phase)
    $localComparable = @($Local | ForEach-Object {
            [pscustomobject][ordered]@{ name = $_.name; size = [long]$_.size; digest = [string]$_.digest }
        } | Sort-Object name)
    $remoteComparable = @($Remote | ForEach-Object {
            [pscustomobject][ordered]@{ name = $_.name; size = [long]$_.size; digest = [string]$_.digest }
        } | Sort-Object name)
    if (($localComparable | ConvertTo-Json -Compress) -cne ($remoteComparable | ConvertTo-Json -Compress)) {
        throw "$Phase release asset IDs/names/sizes/digests do not exactly match the verified local files."
    }
}

$productNames = @(
    "Pusula_${ExpectedVersion}_x64_offline-setup.exe",
    "Pusula_${ExpectedVersion}_x64-setup.exe",
    "Pusula_${ExpectedVersion}_x64-setup.exe.sig",
    'latest.json',
    'SHA256SUMS.txt'
) | Sort-Object
$expectedNames = @($productNames)
if ($Mode -ceq 'Stable') {
    if ($AcceptanceEvidenceAssetName -cne "Pusula_${ExpectedVersion}_acceptance-evidence.json" -or
        $AcceptanceEvidenceSha256 -cnotmatch '^[0-9a-f]{64}$') {
        throw 'Stable publication requires the exact candidate and acceptance evidence identity.'
    }
    $expectedNames = @($productNames + $AcceptanceEvidenceAssetName | Sort-Object)
}
else {
    if ($BuildInitialAcceptanceBaseline -cnotin @('true', 'false')) {
        throw 'Candidate publication requires the exact initial-baseline flag.'
    }
}
$localAssets = Get-LocalReleaseAssets -Directory $AssetDirectory
if (($localAssets.name -join "`n") -cne ($expectedNames -join "`n")) {
    throw "Local publication allowlist mismatch. Expected $($expectedNames -join ', ')."
}
if ($Mode -ceq 'Stable') {
    $evidence = @($localAssets | Where-Object { $_.name -ceq $AcceptanceEvidenceAssetName })
    if ($evidence.Count -ne 1 -or $evidence[0].digest -cne "sha256:$AcceptanceEvidenceSha256") {
        throw 'Stable acceptance evidence asset does not match the validated digest.'
    }
}

try {
    $env:GH_TOKEN = $adminToken
    $null = Assert-PusulaReleaseRepositoryControls -Repository $Repository
}
finally {
    $env:GH_TOKEN = $writeToken
}

Assert-ExactMainCheckoutAndTag
Assert-CandidateBaselineHistory

if ($Mode -ceq 'Stable') {
    $candidate = Get-ExactReleaseByTag -ExpectedTag $CandidateTag
    if ([bool]$candidate.draft -or -not [bool]$candidate.prerelease -or -not [bool]$candidate.immutable -or
        [string]$candidate.target_commitish -cne $ExpectedCommit) {
        throw 'Accepted candidate is not the exact published immutable prerelease immediately before stable publication.'
    }
    $candidateTagJson = @(& gh api "repos/$Repository/git/ref/tags/$CandidateTag")
    if ($LASTEXITCODE -ne 0) { throw 'Could not verify the accepted candidate tag.' }
    $candidateRef = ($candidateTagJson -join "`n") | ConvertFrom-Json
    if ([string]$candidateRef.object.type -cne 'commit' -or [string]$candidateRef.object.sha -cne $ExpectedCommit) {
        throw 'Accepted candidate tag is not pinned to the release commit.'
    }
}

$draft = Get-ExactReleaseByTag -ExpectedTag $Tag
$releaseId = [long]$draft.id
$expectedPrerelease = $Mode -ceq 'Candidate'
if ($releaseId -le 0 -or -not [bool]$draft.draft -or [bool]$draft.prerelease -ne $expectedPrerelease -or
    [string]$draft.target_commitish -cne $ExpectedCommit) {
    throw 'Release is not the exact numeric private draft expected for publication.'
}
if ($Mode -ceq 'Stable') {
    $expectedBody = "Acceptance evidence SHA-256: $AcceptanceEvidenceSha256`nCandidate: $CandidateTag`nThis draft is resumable only with the same tag, commit, assets, and evidence."
    if ([string]$draft.body -cne $expectedBody) {
        throw 'Stable draft body is not exactly bound to the validated candidate and acceptance evidence.'
    }
}
else {
    $expectedBody = "Candidate commit: $ExpectedCommit`nThis draft is resumable only with the same tag, commit, and five signed assets."
    if ([string]$draft.body -cne $expectedBody) {
        throw 'Candidate draft body is not exactly bound to the validated commit and asset policy.'
    }
}
$null = Assert-ExactMainCheckoutAndTag
$null = Assert-CandidateBaselineHistory
$numericDraftJson = @(& gh api @releaseApiHeader "repos/$Repository/releases/$releaseId")
if ($LASTEXITCODE -ne 0) { throw 'Could not read back the draft by numeric release ID.' }
$numericDraft = ($numericDraftJson -join "`n") | ConvertFrom-Json
if ([long]$numericDraft.id -ne $releaseId -or [string]$numericDraft.tag_name -cne $Tag -or
    -not [bool]$numericDraft.draft -or [bool]$numericDraft.prerelease -ne $expectedPrerelease -or
    [bool]$numericDraft.immutable -or [string]$numericDraft.target_commitish -cne $ExpectedCommit -or
    [string]$numericDraft.body -cne $expectedBody) {
    throw 'Numeric draft readback changed release identity.'
}
$remoteAssets = Get-VerifiedRemoteReleaseAssets -Release $numericDraft
Assert-AssetMapsEqual -Local $localAssets -Remote $remoteAssets -Phase 'Draft'

# GitHub has no documented conditional "publish iff these ref and asset digests" API.
# Keep the exact revalidation and PATCH adjacent in this one write-token process.
$patchArgs = @('api') + $releaseApiHeader + @(
    '--method', 'PATCH',
    "repos/$Repository/releases/$releaseId",
    '-F', 'draft=false',
    '-F', "prerelease=$($expectedPrerelease.ToString().ToLowerInvariant())",
    '-F', "make_latest=$(if ($Mode -ceq 'Stable') { 'true' } else { 'false' })"
)
$publishedJson = @(& gh @patchArgs)
if ($LASTEXITCODE -ne 0) {
    throw 'Validated draft could not be published; preserve its numeric ID and rerun the exact validation path.'
}
$published = ($publishedJson -join "`n") | ConvertFrom-Json
if ([long]$published.id -ne $releaseId -or [string]$published.tag_name -cne $Tag -or
    [bool]$published.draft -or [bool]$published.prerelease -ne $expectedPrerelease -or
    -not [bool]$published.immutable -or [string]$published.target_commitish -cne $ExpectedCommit -or
    [string]$published.body -cne $expectedBody) {
    throw 'Published release did not enter the exact immutable state; preserve it as a release incident.'
}

$readbackJson = @(& gh api @releaseApiHeader "repos/$Repository/releases/$releaseId")
if ($LASTEXITCODE -ne 0) { throw 'Could not read back the immutable release by numeric ID.' }
$readback = ($readbackJson -join "`n") | ConvertFrom-Json
if ([long]$readback.id -ne $releaseId -or [string]$readback.tag_name -cne $Tag -or
    [bool]$readback.draft -or [bool]$readback.prerelease -ne $expectedPrerelease -or
    -not [bool]$readback.immutable -or [string]$readback.target_commitish -cne $ExpectedCommit -or
    [string]$readback.body -cne $expectedBody) {
    throw 'Immutable numeric release readback differs from the validated draft.'
}
$publishedAssets = Get-VerifiedRemoteReleaseAssets -Release $readback
Assert-AssetMapsEqual -Local $localAssets -Remote $publishedAssets -Phase 'Published immutable'

$publishedTagJson = @(& gh api "repos/$Repository/git/ref/tags/$Tag")
if ($LASTEXITCODE -ne 0) { throw 'Could not read back the immutable release tag.' }
$publishedTag = ($publishedTagJson -join "`n") | ConvertFrom-Json
if ([string]$publishedTag.object.type -cne 'commit' -or [string]$publishedTag.object.sha -cne $ExpectedCommit) {
    throw 'Immutable published tag differs from the validated release commit.'
}

$attestationVerified = $false
for ($attempt = 0; $attempt -lt 6; $attempt += 1) {
    $attestation = @(& gh release verify $Tag --repo $Repository --format json 2>$null)
    if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace(($attestation -join "`n"))) {
        $attestationVerified = $true
        break
    }
    if ($attempt -lt 5) { Start-Sleep -Seconds 5 }
}
if (-not $attestationVerified) {
    throw 'GitHub immutable release attestation did not verify; preserve the published release as an incident.'
}

if ($Mode -ceq 'Stable') {
    $latest = @(& gh api "repos/$Repository/releases/latest" --jq '.tag_name')
    if ($LASTEXITCODE -ne 0 -or ($latest -join '').Trim() -cne $Tag) {
        throw 'Published stable release is not the repository latest release.'
    }
}

Write-Output "Published verified immutable $Mode release $Tag (numeric ID $releaseId)."
