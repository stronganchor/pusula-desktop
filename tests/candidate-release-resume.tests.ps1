[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path $PSScriptRoot -Parent
$scriptPath = Join-Path $repoRoot 'scripts\Prepare-ResumableCandidateRelease.ps1'
$repository = 'stronganchor/pusula-desktop'
$version = '0.1.0'
$commit = (& git -C $repoRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Could not read test HEAD.' }
$candidateTag = "v$version-candidate.$commit"
$tempRoot = Join-Path $env:TEMP ('pusula-candidate-resume-tests-' + [Guid]::NewGuid().ToString('N'))
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
        id = $Id; name = $Local.name; size = [long]$Local.size
        digest = $Local.digest; state = 'uploaded'
    }
}

function Get-ReleaseJson {
    $summaries = @($global:PusulaCandidateAssets.Values | Sort-Object name | ForEach-Object {
            [ordered]@{ id = $_.id; name = $_.name; size = $_.size; digest = $_.digest }
        })
    return ([ordered]@{
            id = 41; tag_name = $candidateTag; draft = $true; prerelease = $true; immutable = $false
            target_commitish = $commit; body = $global:PusulaCandidateBody; assets = $summaries
        } | ConvertTo-Json -Depth 8 -Compress)
}

$savedToken = $env:GH_TOKEN
try {
    [IO.Directory]::CreateDirectory($assetDirectory) | Out-Null
    $files = [ordered]@{
        "Pusula_${version}_x64_offline-setup.exe" = 'offline'
        "Pusula_${version}_x64-setup.exe" = 'lean'
        "Pusula_${version}_x64-setup.exe.sig" = 'signature'
        'latest.json' = '{}'
        'SHA256SUMS.txt' = 'sums'
    }
    foreach ($entry in $files.GetEnumerator()) {
        [IO.File]::WriteAllText(
            (Join-Path $assetDirectory $entry.Key),
            [string]$entry.Value,
            [Text.UTF8Encoding]::new($false)
        )
    }
    $localRows = Get-LocalRows

    # Simulate a previous attempt that created the protected tag, then failed before draft creation.
    $global:PusulaCandidateTagCommit = $commit
    $global:PusulaCandidateReleaseExists = $false
    $global:PusulaCandidateBody = ''
    $global:PusulaCandidateAssets = @{}
    $global:PusulaCandidateNextAssetId = 100L
    $global:PusulaCandidateUploads = New-Object 'System.Collections.Generic.List[string]'
    $global:PusulaCandidateCommands = New-Object 'System.Collections.Generic.List[string]'
    $global:PusulaCandidateAdditionalRefs = @()

    function global:gh {
        $global:LASTEXITCODE = 0
        $commandLine = $args -join ' '
        $global:PusulaCandidateCommands.Add($commandLine)
        if ($args[0] -eq 'api' -and $commandLine -like '*matching-refs/tags/*') {
            $refs = @([ordered]@{
                    ref = "refs/tags/$candidateTag"
                    object = [ordered]@{ type = 'commit'; sha = $global:PusulaCandidateTagCommit }
                })
            $refs += @($global:PusulaCandidateAdditionalRefs | ForEach-Object {
                    [ordered]@{
                        ref = [string]$_
                        object = [ordered]@{ type = 'commit'; sha = 'f' * 40 }
                    }
                })
            return ConvertTo-Json -InputObject @($refs) -Depth 5 -Compress
        }
        if ($args[0] -eq 'api' -and $commandLine -like '*releases?per_page=100*') {
            if ($global:PusulaCandidateReleaseExists) { return '41' }
            return
        }
        if ($args[0] -eq 'api' -and $commandLine -like '*releases/assets/*') {
            $id = [long]([regex]::Match($commandLine, 'releases/assets/(?<id>[0-9]+)').Groups['id'].Value)
            $asset = @($global:PusulaCandidateAssets.Values | Where-Object { [long]$_.id -eq $id })
            if ($asset.Count -ne 1) { $global:LASTEXITCODE = 1; return }
            return ($asset[0] | ConvertTo-Json -Compress)
        }
        if ($args[0] -eq 'api' -and $commandLine -like '*releases/41*') { return Get-ReleaseJson }
        if ($args[0] -eq 'release' -and $args[1] -eq 'create') {
            $global:PusulaCandidateReleaseExists = $true
            $notesIndex = [Array]::IndexOf([object[]]$args, '--notes')
            $global:PusulaCandidateBody = [string]$args[$notesIndex + 1]
            return 'https://example.invalid/candidate-draft'
        }
        if ($args[0] -eq 'release' -and $args[1] -eq 'upload') {
            $path = [string]$args[$args.Count - 1]
            $local = Get-Item -LiteralPath $path
            if ($global:PusulaCandidateAssets.ContainsKey($local.Name)) {
                throw "Test detected an attempted candidate clobber: $($local.Name)"
            }
            $global:PusulaCandidateNextAssetId += 1
            $global:PusulaCandidateAssets[$local.Name] = New-RemoteAsset -Local ([pscustomobject]@{
                    name = $local.Name; size = $local.Length
                    digest = "sha256:$((Get-FileHash -Algorithm SHA256 -LiteralPath $local.FullName).Hash.ToLowerInvariant())"
                }) -Id $global:PusulaCandidateNextAssetId
            $global:PusulaCandidateUploads.Add($local.Name)
            return
        }
        throw "Unexpected candidate preparation gh invocation: $commandLine"
    }

    $env:GH_TOKEN = 'write-token'
    & $scriptPath -Repository $repository -Version $version -CandidateTag $candidateTag `
        -ExpectedCommit $commit -AssetDirectory $assetDirectory | Out-Host
    if (-not $global:PusulaCandidateReleaseExists -or $global:PusulaCandidateAssets.Count -ne 5 -or
        $global:PusulaCandidateUploads.Count -ne 5) {
        throw 'Tag-only candidate resume did not create the exact draft and five assets.'
    }

    # Simulate a later failure after only two exact assets reached the draft.
    $global:PusulaCandidateAssets = @{}
    $global:PusulaCandidateUploads.Clear()
    $id = 200L
    foreach ($local in @($localRows | Select-Object -First 2)) {
        $id += 1
        $global:PusulaCandidateAssets[$local.name] = New-RemoteAsset -Local $local -Id $id
    }
    & $scriptPath -Repository $repository -Version $version -CandidateTag $candidateTag `
        -ExpectedCommit $commit -AssetDirectory $assetDirectory | Out-Host
    if ($global:PusulaCandidateUploads.Count -ne 3 -or $global:PusulaCandidateAssets.Count -ne 5) {
        throw 'Partial candidate resume did not upload only the three missing assets.'
    }

    $global:PusulaCandidateUploads.Clear()
    $global:PusulaCandidateAdditionalRefs = @("refs/tags/v$version-candidate.$('f' * 40)")
    Assert-ThrowsLike -Pattern '*ambiguous for version*' -Action {
        & $scriptPath -Repository $repository -Version $version -CandidateTag $candidateTag `
            -ExpectedCommit $commit -AssetDirectory $assetDirectory
    }
    if ($global:PusulaCandidateUploads.Count -ne 0) { throw 'Foreign same-version candidate attempted an upload.' }

    $global:PusulaCandidateAdditionalRefs = @("refs/tags/v$version")
    Assert-ThrowsLike -Pattern '*ambiguous for version*' -Action {
        & $scriptPath -Repository $repository -Version $version -CandidateTag $candidateTag `
            -ExpectedCommit $commit -AssetDirectory $assetDirectory
    }
    if ($global:PusulaCandidateUploads.Count -ne 0) { throw 'Existing stable tag attempted a candidate upload.' }
    $global:PusulaCandidateAdditionalRefs = @()

    $corruptName = [string]$localRows[0].name
    $global:PusulaCandidateAssets[$corruptName].digest = 'sha256:' + ('0' * 64)
    $global:PusulaCandidateUploads.Clear()
    Assert-ThrowsLike -Pattern '*cannot be safely resumed*' -Action {
        & $scriptPath -Repository $repository -Version $version -CandidateTag $candidateTag `
            -ExpectedCommit $commit -AssetDirectory $assetDirectory
    }
    if ($global:PusulaCandidateUploads.Count -ne 0) { throw 'Mismatched candidate draft attempted an upload.' }

    $global:PusulaCandidateTagCommit = '0' * 40
    Assert-ThrowsLike -Pattern '*never retag it*' -Action {
        & $scriptPath -Repository $repository -Version $version -CandidateTag $candidateTag `
            -ExpectedCommit $commit -AssetDirectory $assetDirectory
    }
    if (@($global:PusulaCandidateCommands | Where-Object { $_ -match '(?i)delete|--clobber' }).Count -ne 0) {
        throw 'Candidate preparation issued a destructive GitHub command.'
    }
}
finally {
    $env:GH_TOKEN = $savedToken
    Remove-Item Function:\gh -ErrorAction SilentlyContinue
    foreach ($name in @(
            'PusulaCandidateTagCommit', 'PusulaCandidateReleaseExists', 'PusulaCandidateBody',
            'PusulaCandidateAssets', 'PusulaCandidateNextAssetId', 'PusulaCandidateUploads',
            'PusulaCandidateCommands', 'PusulaCandidateAdditionalRefs'
        )) {
        Remove-Variable $name -Scope Global -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output 'Candidate release resume tests passed.'
