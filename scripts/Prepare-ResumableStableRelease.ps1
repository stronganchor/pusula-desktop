[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Repository,
    [Parameter(Mandatory = $true)][string] $Version,
    [Parameter(Mandatory = $true)][string] $CandidateTag,
    [Parameter(Mandatory = $true)][string] $ExpectedCommit,
    [Parameter(Mandatory = $true)][string] $AssetDirectory,
    [Parameter(Mandatory = $true)][string] $AcceptanceEvidenceAssetName,
    [Parameter(Mandatory = $true)][string] $AcceptanceEvidenceSha256
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' -or
    $Version -notmatch '^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$' -or
    $ExpectedCommit -cnotmatch '^[0-9a-f]{40}$' -or
    $CandidateTag -cne "v$Version-candidate.$ExpectedCommit" -or
    $AcceptanceEvidenceAssetName -cne "Pusula_${Version}_acceptance-evidence.json" -or
    $AcceptanceEvidenceSha256 -cnotmatch '^[0-9a-f]{64}$') {
    throw 'Stable draft preparation identity is invalid.'
}

$stableTag = "v$Version"
$evidenceMarker = "Acceptance evidence SHA-256: $AcceptanceEvidenceSha256"
$expectedBody = "$evidenceMarker`nCandidate: $CandidateTag`nThis draft is resumable only with the same tag, commit, assets, and evidence."
$releaseApiHeader = @('-H', 'X-GitHub-Api-Version: 2026-03-10')
$resolvedAssetDirectory = (Resolve-Path -LiteralPath $AssetDirectory -ErrorAction Stop).Path
$expectedNames = @(
    "Pusula_${Version}_x64_offline-setup.exe",
    "Pusula_${Version}_x64-setup.exe",
    "Pusula_${Version}_x64-setup.exe.sig",
    "Pusula_${Version}_acceptance-evidence.json",
    'latest.json',
    'SHA256SUMS.txt'
) | Sort-Object
$localAssets = @(Get-ChildItem -LiteralPath $resolvedAssetDirectory -File -Force | ForEach-Object {
        if ($_.Length -le 0) { throw "Stable release asset is empty: $($_.Name)" }
        [pscustomobject][ordered]@{
            name = $_.Name
            size = [long]$_.Length
            digest = "sha256:$((Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant())"
            path = $_.FullName
        }
    } | Sort-Object name)
if (@(Get-ChildItem -LiteralPath $resolvedAssetDirectory -Directory -Force).Count -ne 0 -or
    ($localAssets.name -join "`n") -cne ($expectedNames -join "`n")) {
    throw 'Stable release directory must contain exactly the five candidate assets and canonical evidence asset.'
}
$evidence = @($localAssets | Where-Object { $_.name -ceq $AcceptanceEvidenceAssetName })
if ($evidence.Count -ne 1 -or $evidence[0].digest -cne "sha256:$AcceptanceEvidenceSha256") {
    throw 'Canonical acceptance evidence file does not match the validated digest.'
}

function Read-JsonCommand {
    param([string[]] $Arguments, [string] $FailureMessage)
    $rows = @(& gh @Arguments)
    if ($LASTEXITCODE -ne 0 -or $rows.Count -eq 0) { throw $FailureMessage }
    return ($rows -join "`n") | ConvertFrom-Json
}

function Get-ExactReleaseByTag {
    $rows = @(& gh api @releaseApiHeader --paginate "repos/$Repository/releases?per_page=100" --jq ".[] | select(.tag_name == `"$stableTag`") | .id")
    if ($LASTEXITCODE -ne 0) { throw 'Could not enumerate stable releases.' }
    $ids = @($rows | ForEach-Object {
            $id = 0L
            if (-not [long]::TryParse(([string]$_).Trim(), [ref]$id) -or $id -le 0) {
                throw 'Stable release enumeration returned an invalid numeric ID.'
            }
            $id
        })
    if ($ids.Count -gt 1) { throw "Stable release lookup is ambiguous: $stableTag" }
    if ($ids.Count -eq 0) { return $null }
    return Read-JsonCommand -Arguments (@('api') + $releaseApiHeader + @("repos/$Repository/releases/$($ids[0])")) `
        -FailureMessage 'Could not read the resumable stable draft by numeric release ID.'
}

function Get-RemoteAssetRows {
    param($Release)
    $rows = @()
    $ids = New-Object 'System.Collections.Generic.HashSet[long]'
    $names = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
    foreach ($summary in @($Release.assets)) {
        $id = [long]$summary.id
        if ($id -le 0 -or -not $ids.Add($id)) { throw 'Stable draft contains an invalid or duplicate asset ID.' }
        $asset = Read-JsonCommand -Arguments (@('api') + $releaseApiHeader + @("repos/$Repository/releases/assets/$id")) `
            -FailureMessage "Could not read stable draft asset ID $id."
        $name = [string]$asset.name
        $digest = ([string]$asset.digest).ToLowerInvariant()
        if ([long]$asset.id -ne $id -or -not $names.Add($name) -or
            [string]$asset.state -cne 'uploaded' -or $digest -cnotmatch '^sha256:[0-9a-f]{64}$' -or
            [string]$summary.name -cne $name -or [long]$summary.size -ne [long]$asset.size -or
            ([string]$summary.digest).ToLowerInvariant() -cne $digest) {
            throw "Stable draft asset ID $id is incomplete or changed identity."
        }
        $rows += [pscustomobject][ordered]@{
            id = $id
            name = $name
            size = [long]$asset.size
            digest = $digest
        }
    }
    return @($rows | Sort-Object name)
}

$matchingRefs = Read-JsonCommand -Arguments @('api', "repos/$Repository/git/matching-refs/tags/$stableTag") `
    -FailureMessage 'Could not inspect the protected stable tag.'
$exactRefs = @($matchingRefs | Where-Object { [string]$_.ref -ceq "refs/tags/$stableTag" })
if ($exactRefs.Count -gt 1) { throw "Stable tag lookup is ambiguous: $stableTag" }
if ($exactRefs.Count -eq 0) {
    $created = Read-JsonCommand -Arguments @(
        'api', '--method', 'POST', "repos/$Repository/git/refs",
        '-f', "ref=refs/tags/$stableTag", '-f', "sha=$ExpectedCommit"
    ) -FailureMessage 'Could not create the protected stable tag.'
    if ([string]$created.ref -cne "refs/tags/$stableTag" -or
        [string]$created.object.type -cne 'commit' -or [string]$created.object.sha -cne $ExpectedCommit) {
        throw 'Created stable tag is not the exact lightweight tag at the accepted commit.'
    }
}
elseif ([string]$exactRefs[0].object.type -cne 'commit' -or
    [string]$exactRefs[0].object.sha -cne $ExpectedCommit) {
    throw 'Existing protected stable tag is not pinned to the accepted commit; never retag it.'
}

$release = Get-ExactReleaseByTag
if ($null -eq $release) {
    $createRows = @(& gh release create $stableTag --repo $Repository --target $ExpectedCommit `
        --title "Pusula $Version" --notes $expectedBody --draft)
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the resumable stable draft; preserve the protected tag.' }
    $release = Get-ExactReleaseByTag
    if ($null -eq $release) { throw 'Created stable draft could not be found by numeric release ID.' }
}

$releaseId = [long]$release.id
if ($releaseId -le 0 -or [string]$release.tag_name -cne $stableTag -or
    -not [bool]$release.draft -or [bool]$release.prerelease -or [bool]$release.immutable -or
    [string]$release.target_commitish -cne $ExpectedCommit -or
    [string]$release.body -cne $expectedBody) {
    throw 'Existing stable release is not the exact resumable private draft; preserve it as an incident.'
}

$remoteAssets = Get-RemoteAssetRows -Release $release
foreach ($remote in $remoteAssets) {
    $local = @($localAssets | Where-Object { $_.name -ceq $remote.name })
    if ($local.Count -ne 1 -or $local[0].size -ne $remote.size -or $local[0].digest -cne $remote.digest) {
        throw "Existing stable draft asset cannot be safely resumed: $($remote.name)"
    }
}
foreach ($local in $localAssets) {
    if (@($remoteAssets | Where-Object { $_.name -ceq $local.name }).Count -eq 0) {
        & gh release upload $stableTag --repo $Repository $local.path
        if ($LASTEXITCODE -ne 0) {
            throw "Stable upload failed for $($local.name); rerun will verify and resume without clobbering."
        }
    }
}

$readback = Read-JsonCommand -Arguments (@('api') + $releaseApiHeader + @("repos/$Repository/releases/$releaseId")) `
    -FailureMessage 'Could not re-read the prepared stable draft by numeric release ID.'
if ([long]$readback.id -ne $releaseId -or [string]$readback.tag_name -cne $stableTag -or
    -not [bool]$readback.draft -or [bool]$readback.prerelease -or [bool]$readback.immutable -or
    [string]$readback.target_commitish -cne $ExpectedCommit -or
    [string]$readback.body -cne $expectedBody) {
    throw 'Prepared stable draft changed identity or state.'
}
$finalAssets = Get-RemoteAssetRows -Release $readback
$localMap = @($localAssets | ForEach-Object {
        [pscustomobject][ordered]@{ name = $_.name; size = $_.size; digest = $_.digest }
    } | Sort-Object name)
$remoteMap = @($finalAssets | ForEach-Object {
        [pscustomobject][ordered]@{ name = $_.name; size = $_.size; digest = $_.digest }
    } | Sort-Object name)
if (($localMap | ConvertTo-Json -Compress) -cne ($remoteMap | ConvertTo-Json -Compress)) {
    throw 'Prepared stable draft does not contain the exact six local assets.'
}

Write-Output "Prepared resumable stable draft $stableTag (numeric ID $releaseId) without deleting or clobbering assets."
