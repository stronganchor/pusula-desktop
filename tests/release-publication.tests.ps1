[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path $PSScriptRoot -Parent
$prepareScript = Join-Path $repoRoot 'scripts\Prepare-ResumableStableRelease.ps1'
$publishScript = Join-Path $repoRoot 'scripts\Publish-VerifiedRelease.ps1'
$repository = 'stronganchor/pusula-desktop'
$version = '0.1.0'
$commit = (& git -C $repoRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Could not read test HEAD.' }
$candidateTag = "v$version-candidate.$commit"
$stableTag = "v$version"
$evidenceName = "Pusula_${version}_acceptance-evidence.json"
$evidenceHash = 'e' * 64
$tempRoot = Join-Path $env:TEMP ('pusula-release-publication-tests-' + [Guid]::NewGuid().ToString('N'))
$assetDirectory = Join-Path $tempRoot 'assets'

function Assert-ThrowsLike {
    param([scriptblock] $Action, [string] $Pattern)
    try { & $Action }
    catch {
        if ($_.Exception.Message -notlike $Pattern) {
            throw "Expected error like '$Pattern', received '$($_.Exception.Message)'."
        }
        return
    }
    throw "Expected action to fail with an error like '$Pattern'."
}

function Get-LocalRows {
    return @(Get-ChildItem -LiteralPath $assetDirectory -File | ForEach-Object {
            [pscustomobject][ordered]@{
                name = $_.Name
                size = [long]$_.Length
                digest = "sha256:$((Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant())"
                path = $_.FullName
            }
        } | Sort-Object name)
}

function New-RemoteAsset {
    param($Local, [long] $Id)
    return [pscustomobject][ordered]@{
        id = $Id
        name = $Local.name
        size = [long]$Local.size
        digest = $Local.digest
        state = 'uploaded'
    }
}

function ConvertTo-RemoteReleaseJson {
    param(
        [long] $Id,
        [string] $Tag,
        [bool] $Draft,
        [bool] $Prerelease,
        [bool] $Immutable,
        [string] $Body,
        [hashtable] $Assets
    )
    $summaries = @($Assets.Values | Sort-Object name | ForEach-Object {
            [ordered]@{ id = $_.id; name = $_.name; size = $_.size; digest = $_.digest }
        })
    return ([ordered]@{
            id = $Id
            tag_name = $Tag
            draft = $Draft
            prerelease = $Prerelease
            immutable = $Immutable
            target_commitish = $commit
            body = $Body
            assets = $summaries
        } | ConvertTo-Json -Depth 8 -Compress)
}

$savedToken = $env:GH_TOKEN
$savedAdminToken = $env:RELEASE_ADMIN_READ_TOKEN
try {
    [IO.Directory]::CreateDirectory($assetDirectory) | Out-Null
    $files = [ordered]@{
        "Pusula_${version}_x64_offline-setup.exe" = 'offline'
        "Pusula_${version}_x64-setup.exe" = 'lean'
        "Pusula_${version}_x64-setup.exe.sig" = 'signature'
        'latest.json' = '{}'
        'SHA256SUMS.txt' = 'sums'
        $evidenceName = 'temporary'
    }
    foreach ($entry in $files.GetEnumerator()) {
        $text = if ($entry.Key -ceq $evidenceName) { 'evidence' } else { [string]$entry.Value }
        [IO.File]::WriteAllText((Join-Path $assetDirectory $entry.Key), $text, [Text.UTF8Encoding]::new($false))
    }
    $evidenceHash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $assetDirectory $evidenceName)).Hash.ToLowerInvariant()
    $localRows = Get-LocalRows

    # The preparation helper must resume an exact protected tag/draft and upload only missing assets.
    $global:PusulaPrepareTagExists = $false
    $global:PusulaPrepareTagCommit = $commit
    $global:PusulaPrepareReleaseExists = $false
    $global:PusulaPrepareBody = ''
    $global:PusulaPrepareAssets = @{}
    $global:PusulaPrepareNextAssetId = 100L
    $global:PusulaPrepareUploads = New-Object 'System.Collections.Generic.List[string]'
    $global:PusulaPrepareCommands = New-Object 'System.Collections.Generic.List[string]'

    function global:gh {
        $global:LASTEXITCODE = 0
        $commandLine = $args -join ' '
        $global:PusulaPrepareCommands.Add($commandLine)

        if ($args[0] -eq 'api' -and $commandLine -like '*matching-refs/tags/*') {
            if (-not $global:PusulaPrepareTagExists) { return '[]' }
            $ref = [ordered]@{
                ref = "refs/tags/$stableTag"
                object = [ordered]@{ type = 'commit'; sha = $global:PusulaPrepareTagCommit }
            }
            return ConvertTo-Json -InputObject @($ref) -Depth 5 -Compress
        }
        if ($args[0] -eq 'api' -and $commandLine -like '*--method POST*git/refs*') {
            $global:PusulaPrepareTagExists = $true
            return ([ordered]@{
                    ref = "refs/tags/$stableTag"
                    object = [ordered]@{ type = 'commit'; sha = $commit }
                } | ConvertTo-Json -Depth 5 -Compress)
        }
        if ($args[0] -eq 'api' -and $commandLine -like '*releases?per_page=100*') {
            if ($global:PusulaPrepareReleaseExists) { return '42' }
            return
        }
        if ($args[0] -eq 'api' -and $commandLine -like '*releases/assets/*') {
            $id = [long]([regex]::Match($commandLine, 'releases/assets/(?<id>[0-9]+)').Groups['id'].Value)
            $asset = @($global:PusulaPrepareAssets.Values | Where-Object { [long]$_.id -eq $id })
            if ($asset.Count -ne 1) { $global:LASTEXITCODE = 1; return }
            return ($asset[0] | ConvertTo-Json -Compress)
        }
        if ($args[0] -eq 'api' -and $commandLine -like '*releases/42*') {
            return ConvertTo-RemoteReleaseJson -Id 42 -Tag $stableTag -Draft $true -Prerelease $false `
                -Immutable $false -Body $global:PusulaPrepareBody -Assets $global:PusulaPrepareAssets
        }
        if ($args[0] -eq 'release' -and $args[1] -eq 'create') {
            $global:PusulaPrepareReleaseExists = $true
            $notesIndex = [Array]::IndexOf([object[]]$args, '--notes')
            $global:PusulaPrepareBody = [string]$args[$notesIndex + 1]
            return 'https://example.invalid/draft'
        }
        if ($args[0] -eq 'release' -and $args[1] -eq 'upload') {
            $path = [string]$args[$args.Count - 1]
            $local = Get-Item -LiteralPath $path
            if ($global:PusulaPrepareAssets.ContainsKey($local.Name)) {
                throw "Test detected an attempted clobber: $($local.Name)"
            }
            $global:PusulaPrepareNextAssetId += 1
            $row = [pscustomobject][ordered]@{
                name = $local.Name
                size = [long]$local.Length
                digest = "sha256:$((Get-FileHash -Algorithm SHA256 -LiteralPath $local.FullName).Hash.ToLowerInvariant())"
                id = $global:PusulaPrepareNextAssetId
                state = 'uploaded'
            }
            $global:PusulaPrepareAssets[$local.Name] = $row
            $global:PusulaPrepareUploads.Add($local.Name)
            return
        }
        throw "Unexpected preparation gh invocation: $commandLine"
    }

    $env:GH_TOKEN = 'write-token'
    & $prepareScript -Repository $repository -Version $version -CandidateTag $candidateTag `
        -ExpectedCommit $commit -AssetDirectory $assetDirectory -AcceptanceEvidenceAssetName $evidenceName `
        -AcceptanceEvidenceSha256 $evidenceHash | Out-Host
    if (-not $global:PusulaPrepareTagExists -or -not $global:PusulaPrepareReleaseExists -or
        $global:PusulaPrepareAssets.Count -ne 6 -or $global:PusulaPrepareUploads.Count -ne 6) {
        throw 'Initial stable draft preparation did not create the tag/draft and exact six assets.'
    }

    $global:PusulaPrepareAssets = @{}
    $global:PusulaPrepareUploads.Clear()
    $id = 200L
    foreach ($local in @($localRows | Select-Object -First 3)) {
        $id += 1
        $global:PusulaPrepareAssets[$local.name] = New-RemoteAsset -Local $local -Id $id
    }
    & $prepareScript -Repository $repository -Version $version -CandidateTag $candidateTag `
        -ExpectedCommit $commit -AssetDirectory $assetDirectory -AcceptanceEvidenceAssetName $evidenceName `
        -AcceptanceEvidenceSha256 $evidenceHash | Out-Host
    if ($global:PusulaPrepareUploads.Count -ne 3 -or $global:PusulaPrepareAssets.Count -ne 6) {
        throw 'Stable rerun did not upload only the three missing assets.'
    }

    $firstName = [string]$localRows[0].name
    $global:PusulaPrepareAssets[$firstName].digest = 'sha256:' + ('0' * 64)
    $global:PusulaPrepareUploads.Clear()
    Assert-ThrowsLike -Pattern '*cannot be safely resumed*' -Action {
        & $prepareScript -Repository $repository -Version $version -CandidateTag $candidateTag `
            -ExpectedCommit $commit -AssetDirectory $assetDirectory -AcceptanceEvidenceAssetName $evidenceName `
            -AcceptanceEvidenceSha256 $evidenceHash
    }
    if ($global:PusulaPrepareUploads.Count -ne 0) { throw 'Mismatched resumable draft attempted an upload.' }
    $global:PusulaPrepareTagCommit = '0' * 40
    Assert-ThrowsLike -Pattern '*never retag it*' -Action {
        & $prepareScript -Repository $repository -Version $version -CandidateTag $candidateTag `
            -ExpectedCommit $commit -AssetDirectory $assetDirectory -AcceptanceEvidenceAssetName $evidenceName `
            -AcceptanceEvidenceSha256 $evidenceHash
    }
    if (@($global:PusulaPrepareCommands | Where-Object { $_ -match '(?i)delete|--clobber' }).Count -ne 0) {
        throw 'Stable preparation issued a destructive GitHub command.'
    }

    Remove-Item Function:\gh -ErrorAction Stop

    # The final publication helper must validate numeric asset IDs, PATCH once, and verify immutable readback.
    $global:PusulaPublishStableDraft = $true
    $global:PusulaPublishPatchCount = 0
    $global:PusulaPublishCommands = New-Object 'System.Collections.Generic.List[string]'
    $global:PusulaPublishCandidateAssets = @{}
    $global:PusulaPublishStableAssets = @{}
    $global:PusulaPublishStableReadCount = 0
    $global:PusulaPublishMutateNumericDraft = $false
    $id = 300L
    foreach ($local in $localRows) {
        $id += 1
        $remote = New-RemoteAsset -Local $local -Id $id
        $global:PusulaPublishStableAssets[$local.name] = $remote
        if ($local.name -cne $evidenceName) { $global:PusulaPublishCandidateAssets[$local.name] = $remote }
    }
    $evidenceMarker = "Acceptance evidence SHA-256: $evidenceHash`nCandidate: $candidateTag`nThis draft is resumable only with the same tag, commit, assets, and evidence."

    function global:gh {
        $global:LASTEXITCODE = 0
        $commandLine = $args -join ' '
        $global:PusulaPublishCommands.Add($commandLine)
        if ($commandLine -like '*immutable-releases*') { return '{"enabled":true}' }
        if ($commandLine -like '*rulesets/18968971*') {
            return (@{
                    id = 18968971; name = 'Protect release tags'; target = 'tag'; enforcement = 'active'
                    conditions = @{ ref_name = @{ include = @('refs/tags/v*'); exclude = @() } }
                    rules = @(@{ type = 'update' }, @{ type = 'deletion' })
                    bypass_actors = @(); current_user_can_bypass = 'never'
                } | ConvertTo-Json -Depth 8 -Compress)
        }
        if ($commandLine -like '*repos/stronganchor/pusula-desktop/rulesets*') {
            return ConvertTo-Json -InputObject @(@{ id = 18968971; name = 'Protect release tags'; target = 'tag' }) -Compress
        }
        if ($commandLine -like '*git/ref/heads/main*') {
            return (@{ object = @{ sha = $commit } } | ConvertTo-Json -Compress)
        }
        if ($commandLine -like '*git/ref/tags/*') {
            $tag = if ($commandLine -like "*$candidateTag*") { $candidateTag } else { $stableTag }
            return (@{ ref = "refs/tags/$tag"; object = @{ type = 'commit'; sha = $commit } } | ConvertTo-Json -Compress)
        }
        if ($commandLine -like '*releases?per_page=100*') {
            if ($commandLine -like "*$candidateTag*") { return '10' }
            if ($commandLine -like "*$stableTag*") { return '20' }
            return
        }
        if ($commandLine -like '*releases/assets/*') {
            $assetId = [long]([regex]::Match($commandLine, 'releases/assets/(?<id>[0-9]+)').Groups['id'].Value)
            $asset = @($global:PusulaPublishStableAssets.Values | Where-Object { [long]$_.id -eq $assetId })
            if ($asset.Count -ne 1) { $global:LASTEXITCODE = 1; return }
            return ($asset[0] | ConvertTo-Json -Compress)
        }
        if ($commandLine -like '*--method PATCH*releases/20*') {
            $global:PusulaPublishPatchCount += 1
            $global:PusulaPublishStableDraft = $false
            return ConvertTo-RemoteReleaseJson -Id 20 -Tag $stableTag -Draft $false -Prerelease $false `
                -Immutable $true -Body $evidenceMarker -Assets $global:PusulaPublishStableAssets
        }
        if ($commandLine -like '*releases/10*') {
            return ConvertTo-RemoteReleaseJson -Id 10 -Tag $candidateTag -Draft $false -Prerelease $true `
                -Immutable $true -Body '' -Assets $global:PusulaPublishCandidateAssets
        }
        if ($commandLine -like '*releases/20*') {
            $global:PusulaPublishStableReadCount += 1
            $body = $evidenceMarker
            if ($global:PusulaPublishMutateNumericDraft -and
                $global:PusulaPublishStableDraft -and
                $global:PusulaPublishStableReadCount -ge 2) {
                $body = 'mutated after the first draft validation'
            }
            return ConvertTo-RemoteReleaseJson -Id 20 -Tag $stableTag -Draft $global:PusulaPublishStableDraft `
                -Prerelease $false -Immutable (-not $global:PusulaPublishStableDraft) -Body $body `
                -Assets $global:PusulaPublishStableAssets
        }
        if ($args[0] -eq 'release' -and $args[1] -eq 'verify') { return '{"verified":true}' }
        if ($commandLine -like '*releases/latest*') { return $stableTag }
        throw "Unexpected publication gh invocation: $commandLine"
    }

    $env:GH_TOKEN = 'write-token'
    $env:RELEASE_ADMIN_READ_TOKEN = 'admin-read-token'
    & $publishScript -Mode Stable -Repository $repository -Tag $stableTag -ExpectedCommit $commit `
        -ExpectedVersion $version -AssetDirectory $assetDirectory -CandidateTag $candidateTag `
        -AcceptanceEvidenceAssetName $evidenceName -AcceptanceEvidenceSha256 $evidenceHash | Out-Host
    if ($global:PusulaPublishPatchCount -ne 1 -or $global:PusulaPublishStableDraft) {
        throw 'Verified stable publication did not issue exactly one successful PATCH.'
    }
    if (@($global:PusulaPublishCommands | Where-Object { $_ -match '(?i)delete|--clobber' }).Count -ne 0) {
        throw 'Publication helper issued a destructive GitHub command.'
    }

    $global:PusulaPublishStableDraft = $true
    $global:PusulaPublishPatchCount = 0
    $global:PusulaPublishStableReadCount = 0
    $corruptName = [string]$localRows[0].name
    $correctDigest = $global:PusulaPublishStableAssets[$corruptName].digest
    $global:PusulaPublishStableAssets[$corruptName].digest = 'sha256:' + ('9' * 64)
    Assert-ThrowsLike -Pattern '*do not exactly match the verified local files*' -Action {
        & $publishScript -Mode Stable -Repository $repository -Tag $stableTag -ExpectedCommit $commit `
            -ExpectedVersion $version -AssetDirectory $assetDirectory -CandidateTag $candidateTag `
            -AcceptanceEvidenceAssetName $evidenceName -AcceptanceEvidenceSha256 $evidenceHash
    }
    if ($global:PusulaPublishPatchCount -ne 0) { throw 'Digest mismatch reached the publication PATCH.' }
    $global:PusulaPublishStableAssets[$corruptName].digest = $correctDigest

    $global:PusulaPublishStableDraft = $true
    $global:PusulaPublishPatchCount = 0
    $global:PusulaPublishStableReadCount = 0
    $global:PusulaPublishMutateNumericDraft = $true
    Assert-ThrowsLike -Pattern '*Numeric draft readback changed release identity*' -Action {
        & $publishScript -Mode Stable -Repository $repository -Tag $stableTag -ExpectedCommit $commit `
            -ExpectedVersion $version -AssetDirectory $assetDirectory -CandidateTag $candidateTag `
            -AcceptanceEvidenceAssetName $evidenceName -AcceptanceEvidenceSha256 $evidenceHash
    }
    if ($global:PusulaPublishPatchCount -ne 0) { throw 'Draft body race mutation reached the publication PATCH.' }
    $global:PusulaPublishMutateNumericDraft = $false
}
finally {
    $env:GH_TOKEN = $savedToken
    $env:RELEASE_ADMIN_READ_TOKEN = $savedAdminToken
    Remove-Item Function:\gh -ErrorAction SilentlyContinue
    foreach ($name in @(
            'PusulaPrepareTagExists', 'PusulaPrepareTagCommit', 'PusulaPrepareReleaseExists',
            'PusulaPrepareBody', 'PusulaPrepareAssets', 'PusulaPrepareNextAssetId',
            'PusulaPrepareUploads', 'PusulaPrepareCommands', 'PusulaPublishStableDraft',
            'PusulaPublishPatchCount', 'PusulaPublishCommands', 'PusulaPublishCandidateAssets',
            'PusulaPublishStableAssets', 'PusulaPublishStableReadCount',
            'PusulaPublishMutateNumericDraft'
        )) {
        Remove-Variable $name -Scope Global -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output 'Release publication and stable-resume tests passed.'
