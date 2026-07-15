[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Repository,
    [Parameter(Mandatory = $true)][string] $Version,
    [Parameter(Mandatory = $true)][string] $CandidateTag,
    [Parameter(Mandatory = $true)][string] $ExpectedCommit,
    [Parameter(Mandatory = $true)][string] $AssetDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' -or
    $Version -notmatch '^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$' -or
    $ExpectedCommit -cnotmatch '^[0-9a-f]{40}$' -or
    $CandidateTag -cne "v$Version-candidate.$ExpectedCommit") {
    throw 'Candidate draft preparation identity is invalid.'
}

$expectedBody = "Candidate commit: $ExpectedCommit`nThis draft is resumable only with the same tag, commit, and five signed assets."
$releaseApiHeader = @('-H', 'X-GitHub-Api-Version: 2026-03-10')
$resolvedAssetDirectory = (Resolve-Path -LiteralPath $AssetDirectory -ErrorAction Stop).Path
$expectedNames = @(
    "Pusula_${Version}_x64_offline-setup.exe",
    "Pusula_${Version}_x64-setup.exe",
    "Pusula_${Version}_x64-setup.exe.sig",
    'latest.json',
    'SHA256SUMS.txt'
) | Sort-Object
$localAssets = @(Get-ChildItem -LiteralPath $resolvedAssetDirectory -File -Force | ForEach-Object {
        if ($_.Length -le 0) { throw "Candidate release asset is empty: $($_.Name)" }
        [pscustomobject][ordered]@{
            name = $_.Name
            size = [long]$_.Length
            digest = "sha256:$((Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant())"
            path = $_.FullName
        }
    } | Sort-Object name)
if (@(Get-ChildItem -LiteralPath $resolvedAssetDirectory -Directory -Force).Count -ne 0 -or
    ($localAssets.name -join "`n") -cne ($expectedNames -join "`n")) {
    throw 'Candidate release directory must contain exactly the five signed candidate assets.'
}

function Read-JsonCommand {
    param([string[]] $Arguments, [string] $FailureMessage)
    $rows = @(& gh @Arguments)
    if ($LASTEXITCODE -ne 0 -or $rows.Count -eq 0) { throw $FailureMessage }
    return ($rows -join "`n") | ConvertFrom-Json
}

function Get-ExactReleaseByTag {
    $rows = @(& gh api @releaseApiHeader --paginate "repos/$Repository/releases?per_page=100" --jq ".[] | select(.tag_name == `"$CandidateTag`") | .id")
    if ($LASTEXITCODE -ne 0) { throw 'Could not enumerate candidate releases.' }
    $ids = @($rows | ForEach-Object {
            $id = 0L
            if (-not [long]::TryParse(([string]$_).Trim(), [ref]$id) -or $id -le 0) {
                throw 'Candidate release enumeration returned an invalid numeric ID.'
            }
            $id
        })
    if ($ids.Count -gt 1) { throw "Candidate release lookup is ambiguous: $CandidateTag" }
    if ($ids.Count -eq 0) { return $null }
    return Read-JsonCommand -Arguments (@('api') + $releaseApiHeader + @("repos/$Repository/releases/$($ids[0])")) `
        -FailureMessage 'Could not read the resumable candidate draft by numeric release ID.'
}

function Get-RemoteAssetRows {
    param($Release)
    $rows = @()
    $ids = New-Object 'System.Collections.Generic.HashSet[long]'
    $names = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
    foreach ($summary in @($Release.assets)) {
        $id = [long]$summary.id
        if ($id -le 0 -or -not $ids.Add($id)) { throw 'Candidate draft contains an invalid or duplicate asset ID.' }
        $asset = Read-JsonCommand -Arguments (@('api') + $releaseApiHeader + @("repos/$Repository/releases/assets/$id")) `
            -FailureMessage "Could not read candidate draft asset ID $id."
        $name = [string]$asset.name
        $digest = ([string]$asset.digest).ToLowerInvariant()
        if ([long]$asset.id -ne $id -or -not $names.Add($name) -or
            [string]$asset.state -cne 'uploaded' -or $digest -cnotmatch '^sha256:[0-9a-f]{64}$' -or
            [string]$summary.name -cne $name -or [long]$summary.size -ne [long]$asset.size -or
            ([string]$summary.digest).ToLowerInvariant() -cne $digest) {
            throw "Candidate draft asset ID $id is incomplete or changed identity."
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

$matchingRefs = Read-JsonCommand -Arguments @('api', "repos/$Repository/git/matching-refs/tags/v$Version") `
    -FailureMessage 'Could not inspect protected tags for the candidate version.'
$exactRefs = @($matchingRefs | Where-Object { [string]$_.ref -ceq "refs/tags/$CandidateTag" })
$conflictingVersionRefs = @($matchingRefs | Where-Object {
        $name = [string]$_.ref
        ($name -ceq "refs/tags/v$Version" -or
            $name.StartsWith("refs/tags/v$Version-candidate.", [StringComparison]::Ordinal)) -and
        $name -cne "refs/tags/$CandidateTag"
    })
if ($exactRefs.Count -gt 1 -or $conflictingVersionRefs.Count -ne 0) {
    throw "Candidate tag lookup is ambiguous for version $Version."
}
if ($exactRefs.Count -eq 0) {
    $created = Read-JsonCommand -Arguments @(
        'api', '--method', 'POST', "repos/$Repository/git/refs",
        '-f', "ref=refs/tags/$CandidateTag", '-f', "sha=$ExpectedCommit"
    ) -FailureMessage 'Could not create the protected candidate tag.'
    if ([string]$created.ref -cne "refs/tags/$CandidateTag" -or
        [string]$created.object.type -cne 'commit' -or [string]$created.object.sha -cne $ExpectedCommit) {
        throw 'Created candidate tag is not the exact lightweight tag at the built commit.'
    }
}
elseif ([string]$exactRefs[0].object.type -cne 'commit' -or
    [string]$exactRefs[0].object.sha -cne $ExpectedCommit) {
    throw 'Existing protected candidate tag is not pinned to the built commit; never retag it.'
}

$release = Get-ExactReleaseByTag
if ($null -eq $release) {
    $createRows = @(& gh release create $CandidateTag --repo $Repository --target $ExpectedCommit `
        --title "Pusula $Version acceptance candidate" --notes $expectedBody --draft --prerelease)
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the resumable candidate draft; preserve the protected tag.' }
    $release = Get-ExactReleaseByTag
    if ($null -eq $release) { throw 'Created candidate draft could not be found by numeric release ID.' }
}

$releaseId = [long]$release.id
if ($releaseId -le 0 -or [string]$release.tag_name -cne $CandidateTag -or
    -not [bool]$release.draft -or -not [bool]$release.prerelease -or [bool]$release.immutable -or
    [string]$release.target_commitish -cne $ExpectedCommit -or [string]$release.body -cne $expectedBody) {
    throw 'Existing candidate release is not the exact resumable private draft; preserve it as an incident.'
}

$remoteAssets = Get-RemoteAssetRows -Release $release
foreach ($remote in $remoteAssets) {
    $local = @($localAssets | Where-Object { $_.name -ceq $remote.name })
    if ($local.Count -ne 1 -or $local[0].size -ne $remote.size -or $local[0].digest -cne $remote.digest) {
        throw "Existing candidate draft asset cannot be safely resumed: $($remote.name)"
    }
}
foreach ($local in $localAssets) {
    if (@($remoteAssets | Where-Object { $_.name -ceq $local.name }).Count -eq 0) {
        & gh release upload $CandidateTag --repo $Repository $local.path
        if ($LASTEXITCODE -ne 0) {
            throw "Candidate upload failed for $($local.name); rerun failed jobs to verify and resume without clobbering."
        }
    }
}

$readback = Read-JsonCommand -Arguments (@('api') + $releaseApiHeader + @("repos/$Repository/releases/$releaseId")) `
    -FailureMessage 'Could not re-read the prepared candidate draft by numeric release ID.'
if ([long]$readback.id -ne $releaseId -or [string]$readback.tag_name -cne $CandidateTag -or
    -not [bool]$readback.draft -or -not [bool]$readback.prerelease -or [bool]$readback.immutable -or
    [string]$readback.target_commitish -cne $ExpectedCommit -or [string]$readback.body -cne $expectedBody) {
    throw 'Prepared candidate draft changed identity or state.'
}
$finalAssets = Get-RemoteAssetRows -Release $readback
$localMap = @($localAssets | ForEach-Object {
        [pscustomobject][ordered]@{ name = $_.name; size = $_.size; digest = $_.digest }
    } | Sort-Object name)
$remoteMap = @($finalAssets | ForEach-Object {
        [pscustomobject][ordered]@{ name = $_.name; size = $_.size; digest = $_.digest }
    } | Sort-Object name)
if (($localMap | ConvertTo-Json -Compress) -cne ($remoteMap | ConvertTo-Json -Compress)) {
    throw 'Prepared candidate draft does not contain the exact five local assets.'
}

Write-Output "Prepared resumable candidate draft $CandidateTag (numeric ID $releaseId) without deleting or clobbering assets."
