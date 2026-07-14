[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path $PSScriptRoot -Parent
$identityScript = Join-Path $repoRoot 'scripts\Test-ReleaseIdentity.ps1'
$promotionScript = Join-Path $repoRoot 'scripts\Test-CandidatePromotion.ps1'
$candidateConfigScript = Join-Path $repoRoot 'scripts\New-CandidateUpdaterConfig.ps1'
$manifestScript = Join-Path $repoRoot 'scripts\New-UpdateManifest.ps1'
$assetScript = Join-Path $repoRoot 'scripts\Test-ReleaseAssets.ps1'
$package = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'package.json') | ConvertFrom-Json
$version = [string]$package.version
$commit = (& git -C $repoRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $commit -cnotmatch '^[0-9a-f]{40}$') {
    throw 'Could not determine the release-policy test commit.'
}
$candidateTag = "v$version-candidate.$commit"
$ciWorkflow = Get-Content -Raw -LiteralPath (Join-Path $repoRoot '.github\workflows\ci.yml')
$releaseWorkflow = Get-Content -Raw -LiteralPath (Join-Path $repoRoot '.github\workflows\release.yml')
$promotionWorkflow = Get-Content -Raw -LiteralPath (Join-Path $repoRoot '.github\workflows\promote-release.yml')
$tauriConfig = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'src-tauri\tauri.conf.json') | ConvertFrom-Json
if ([string]$tauriConfig.bundle.windows.webviewInstallMode.type -cne 'offlineInstaller') {
    throw 'Release installer must embed the offline WebView2 installer.'
}
$updaterArtifacts = $tauriConfig.bundle.PSObject.Properties['createUpdaterArtifacts']
if ($null -eq $updaterArtifacts -or
    $updaterArtifacts.Value -isnot [bool] -or
    -not [bool]$updaterArtifacts.Value) {
    throw 'The base Tauri configuration must use modern v2 updater artifacts.'
}
if ([string]$tauriConfig.bundle.windows.nsis.installMode -cne 'currentUser') {
    throw 'Release installer must retain current-user installation mode.'
}
$allowDowngrades = $tauriConfig.bundle.windows.PSObject.Properties['allowDowngrades']
if ($null -eq $allowDowngrades -or
    $allowDowngrades.Value -isnot [bool] -or
    [bool]$allowDowngrades.Value) {
    throw 'Release installer must reject downgrades because SQLite migrations are forward-only.'
}

function Assert-ThrowsLike {
    param(
        [Parameter(Mandatory = $true)][scriptblock] $Action,
        [Parameter(Mandatory = $true)][string] $Pattern
    )

    try {
        & $Action
    }
    catch {
        if ($_.Exception.Message -notlike $Pattern) {
            throw "Expected error like '$Pattern', received '$($_.Exception.Message)'."
        }
        return
    }

    throw "Expected action to fail with an error like '$Pattern'."
}

function Assert-OrderedWorkflowSteps {
    param(
        [Parameter(Mandatory = $true)][string] $Workflow,
        [Parameter(Mandatory = $true)][string[]] $StepNames
    )

    $previous = -1
    foreach ($stepName in $StepNames) {
        $index = $Workflow.IndexOf("- name: $stepName", [StringComparison]::Ordinal)
        if ($index -le $previous) {
            throw "Release workflow step is missing or out of order: $stepName"
        }
        $previous = $index
    }
}

function Test-OrdinalContains {
    param(
        [Parameter(Mandatory = $true)][string] $Text,
        [Parameter(Mandatory = $true)][string] $Value
    )

    return $Text.IndexOf($Value, [StringComparison]::Ordinal) -ge 0
}

$bundleOverrideStart = $releaseWorkflow.IndexOf('- name: Create deterministic bundle overrides', [StringComparison]::Ordinal)
$bundleOverrideEnd = $releaseWorkflow.IndexOf('- name: Require release signing configuration', [StringComparison]::Ordinal)
if ($bundleOverrideStart -lt 0 -or $bundleOverrideEnd -le $bundleOverrideStart) {
    throw 'Release workflow bundle override step is missing or out of order.'
}
$bundleOverrides = $releaseWorkflow.Substring($bundleOverrideStart, $bundleOverrideEnd - $bundleOverrideStart)
if (-not (Test-OrdinalContains -Text $bundleOverrides -Value 'createUpdaterArtifacts = $false') -or
    -not (Test-OrdinalContains -Text $bundleOverrides -Value 'createUpdaterArtifacts = $true') -or
    -not (Test-OrdinalContains -Text $bundleOverrides -Value "type = 'downloadBootstrapper'") -or
    (Test-OrdinalContains -Text $bundleOverrides -Value "type = 'skip'") -or
    (Test-OrdinalContains -Text $bundleOverrides -Value 'v1Compatible')) {
    throw 'Release workflow must build a non-updater offline installer and a modern direct-EXE online updater.'
}
if (-not (Test-OrdinalContains -Text $ciWorkflow -Value '- name: Package Tauri v2 lean updater smoke artifact') -or
    -not (Test-OrdinalContains -Text $ciWorkflow -Value 'Tauri v2 direct NSIS updater signature is missing.') -or
    -not (Test-OrdinalContains -Text $ciWorkflow -Value 'Deprecated Tauri v1-compatible NSIS updater ZIP was generated.')) {
    throw 'CI must exercise the modern direct-EXE NSIS updater bundle path.'
}

Assert-OrderedWorkflowSteps -Workflow $releaseWorkflow -StepNames @(
    'Create exact candidate tag and upload a private draft',
    'Recheck immutable policy, exact tag, identity, and draft bytes immediately before publication',
    'Publish the revalidated candidate'
)
Assert-OrderedWorkflowSteps -Workflow $promotionWorkflow -StepNames @(
    'Create exact stable tag and upload a private draft',
    'Recheck immutable policy, candidate, exact stable tag, and draft bytes immediately before publication',
    'Publish the revalidated stable release'
)
if (-not (Test-OrdinalContains -Text $releaseWorkflow -Value '"ref=refs/tags/$tag"') -or
    -not (Test-OrdinalContains -Text $releaseWorkflow -Value '"sha=$($env:RELEASE_COMMIT)"') -or
    ([regex]::Matches($releaseWorkflow, [regex]::Escape('git/ref/tags/$tag'))).Count -lt 4) {
    throw 'Candidate workflow must exclusively create and repeatedly verify its exact lightweight Git tag.'
}
if (-not (Test-OrdinalContains -Text $promotionWorkflow -Value '"ref=refs/tags/$stableTag"') -or
    -not (Test-OrdinalContains -Text $promotionWorkflow -Value '"sha=$candidateCommit"') -or
    ([regex]::Matches($promotionWorkflow, [regex]::Escape('git/ref/tags/$stableTag'))).Count -lt 4) {
    throw 'Stable workflow must exclusively create and repeatedly verify its exact lightweight Git tag.'
}
$candidateRecheckStart = $releaseWorkflow.IndexOf('- name: Recheck immutable policy, exact tag, identity, and draft bytes immediately before publication', [StringComparison]::Ordinal)
$candidatePublishStart = $releaseWorkflow.IndexOf('- name: Publish the revalidated candidate', [StringComparison]::Ordinal)
$candidateRecheck = $releaseWorkflow.Substring($candidateRecheckStart, $candidatePublishStart - $candidateRecheckStart)
if (-not (Test-OrdinalContains -Text $candidateRecheck -Value 'GH_TOKEN: ${{ secrets.RELEASE_ADMIN_READ_TOKEN }}') -or
    -not (Test-OrdinalContains -Text $candidateRecheck -Value '$assets = @(Get-ChildItem -LiteralPath release-assets -File | Sort-Object Name)') -or
    -not (Test-OrdinalContains -Text $candidateRecheck -Value 'immutable-releases') -or
    -not (Test-OrdinalContains -Text $candidateRecheck -Value ') -cne ($remoteAssets | ConvertTo-Json -Compress)')) {
    throw 'Candidate workflow must reconstruct and case-sensitively revalidate draft assets and immutability with the read-only admin token.'
}
$stableRecheckStart = $promotionWorkflow.IndexOf('- name: Recheck immutable policy, candidate, exact stable tag, and draft bytes immediately before publication', [StringComparison]::Ordinal)
$stablePublishStart = $promotionWorkflow.IndexOf('- name: Publish the revalidated stable release', [StringComparison]::Ordinal)
$stableRecheck = $promotionWorkflow.Substring($stableRecheckStart, $stablePublishStart - $stableRecheckStart)
if (-not (Test-OrdinalContains -Text $stableRecheck -Value 'GH_TOKEN: ${{ secrets.RELEASE_ADMIN_READ_TOKEN }}') -or
    -not (Test-OrdinalContains -Text $stableRecheck -Value 'immutable-releases') -or
    -not (Test-OrdinalContains -Text $stableRecheck -Value ') -cne ($remoteAssets | ConvertTo-Json -Compress)')) {
    throw 'Stable workflow must case-sensitively revalidate draft assets and immutability with the read-only admin token.'
}
$candidatePublish = $releaseWorkflow.Substring($candidatePublishStart)
if (-not (Test-OrdinalContains -Text $candidatePublish -Value 'GH_TOKEN: ${{ github.token }}') -or
    -not (Test-OrdinalContains -Text $candidatePublish -Value 'git/ref/tags/$tag') -or
    -not (Test-OrdinalContains -Text $candidatePublish -Value ') -cne ($publishedAssets | ConvertTo-Json -Compress)')) {
    throw 'Candidate publication must use the isolated write token and read back its exact immutable tag and bytes.'
}
$stablePublish = $promotionWorkflow.Substring($stablePublishStart)
if (-not (Test-OrdinalContains -Text $stablePublish -Value 'GH_TOKEN: ${{ github.token }}') -or
    -not (Test-OrdinalContains -Text $stablePublish -Value 'git/ref/tags/$stableTag') -or
    -not (Test-OrdinalContains -Text $stablePublish -Value ') -cne ($publishedAssets | ConvertTo-Json -Compress)')) {
    throw 'Stable publication must use the isolated write token and read back its exact immutable tag and bytes.'
}

$originalRef = $env:GITHUB_REF
$global:PusulaReleasePolicyVersion = $version
$global:PusulaReleasePolicyCommit = $commit
$global:PusulaReleasePolicyCandidateTag = $candidateTag
$global:PusulaReleasePolicyImmutabilityEnabled = $true
$global:PusulaReleasePolicyCandidateImmutable = $true
$global:PusulaReleasePolicyStableTagExists = $false
$global:PusulaReleasePolicyStableReleaseExists = $false

function global:gh {
    $global:LASTEXITCODE = 0
    $commandLine = $args -join ' '

    if ($commandLine -like '*immutable-releases*') {
        return (@{
                enabled = [bool]$global:PusulaReleasePolicyImmutabilityEnabled
                enforced_by_owner = $false
            } | ConvertTo-Json -Compress)
    }
    if ($commandLine -like '*git/ref/heads/main*') {
        return (@{ object = @{ sha = $global:PusulaReleasePolicyCommit } } | ConvertTo-Json -Compress)
    }
    if ($commandLine -like '*matching-refs/tags/*') {
        if ($global:PusulaReleasePolicyStableTagExists) {
            return (@(@{
                        ref = "refs/tags/v$($global:PusulaReleasePolicyVersion)"
                        object = @{ sha = $global:PusulaReleasePolicyCommit; type = 'commit' }
                    }) | ConvertTo-Json -Compress)
        }
        return '[]'
    }
    if ($commandLine -like '*releases?per_page=100*') {
        if ($global:PusulaReleasePolicyStableReleaseExists -and $commandLine -like '*.tag_name*') {
            return "v$($global:PusulaReleasePolicyVersion)"
        }
        return
    }
    if ($args.Count -ge 2 -and $args[0] -eq 'release' -and $args[1] -eq 'view') {
        return (@{
                tagName = $global:PusulaReleasePolicyCandidateTag
                isDraft = $false
                isImmutable = [bool]$global:PusulaReleasePolicyCandidateImmutable
                isPrerelease = $true
                targetCommitish = $global:PusulaReleasePolicyCommit
                assets = @()
            } | ConvertTo-Json -Compress)
    }

    throw "Unexpected mocked gh invocation: $commandLine"
}

try {
    $env:GITHUB_REF = 'refs/heads/main'

    Assert-ThrowsLike -Pattern '*must use the final SemVer without a prerelease suffix*' -Action {
        & $identityScript `
            -ExpectedVersion "$version-rc.1" `
            -Repository 'stronganchor/pusula-desktop' `
            -ExpectedCommit $commit `
            -CandidateTag $candidateTag
    }

    Assert-ThrowsLike -Pattern '*Candidate tag must exactly equal*' -Action {
        & $identityScript `
            -ExpectedVersion $version `
            -Repository 'stronganchor/pusula-desktop' `
            -ExpectedCommit $commit `
            -CandidateTag "v$version-candidate.$('0' * 40)"
    }

    & $identityScript `
        -ExpectedVersion $version `
        -Repository 'stronganchor/pusula-desktop' `
        -ExpectedCommit $commit `
        -CandidateTag $candidateTag | Out-Host

    $fixtureDirectory = Join-Path $env:TEMP ('pusula-release-policy-fixture-' + [Guid]::NewGuid().ToString('N'))
    try {
        [IO.Directory]::CreateDirectory($fixtureDirectory) | Out-Null
        $candidateConfigPath = Join-Path $fixtureDirectory 'candidate-config.json'
        & $candidateConfigScript `
            -CandidateVersion $version `
            -CandidateTag $candidateTag `
            -Repository 'stronganchor/pusula-desktop' `
            -OutputPath $candidateConfigPath | Out-Host
        $candidateConfig = Get-Content -Raw -LiteralPath $candidateConfigPath | ConvertFrom-Json
        $expectedManifestUrl = "https://github.com/stronganchor/pusula-desktop/releases/download/$candidateTag/latest.json"
        if ([string]$candidateConfig.plugins.updater.endpoints[0] -cne $expectedManifestUrl) {
            throw 'Candidate updater override did not retain the immutable candidate tag.'
        }
        Remove-Item -LiteralPath $candidateConfigPath -Force

        $updaterName = "Pusula_${version}_x64-setup.exe"
        $signatureName = "$updaterName.sig"
        $signaturePath = Join-Path $fixtureDirectory $signatureName
        [IO.File]::WriteAllText((Join-Path $fixtureDirectory "Pusula_${version}_x64_offline-setup.exe"), 'offline-fixture')
        [IO.File]::WriteAllText((Join-Path $fixtureDirectory "Pusula_${version}_x64-setup.exe"), 'lean-fixture')
        [IO.File]::WriteAllText($signaturePath, 'fixture-signature')
        & $manifestScript `
            -Version $version `
            -ReleaseTag $candidateTag `
            -SignaturePath $signaturePath `
            -ArtifactName $updaterName `
            -Repository 'stronganchor/pusula-desktop' `
            -OutputPath (Join-Path $fixtureDirectory 'latest.json')

        $hashRows = @(Get-ChildItem -LiteralPath $fixtureDirectory -File | Sort-Object Name | ForEach-Object {
                $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant()
                "$hash  $($_.Name)"
            })
        [IO.File]::WriteAllLines((Join-Path $fixtureDirectory 'SHA256SUMS.txt'), $hashRows)
        & $assetScript `
            -Directory $fixtureDirectory `
            -Version $version `
            -ReleaseTag $candidateTag `
            -Repository 'stronganchor/pusula-desktop' | Out-Host

        $canonicalInstaller = Join-Path $fixtureDirectory "Pusula_${version}_x64_offline-setup.exe"
        $temporaryInstaller = Join-Path $fixtureDirectory 'case-rename.tmp'
        $caseMutatedInstaller = Join-Path $fixtureDirectory "pusula_${version}_x64_offline-setup.exe"
        Move-Item -LiteralPath $canonicalInstaller -Destination $temporaryInstaller
        Move-Item -LiteralPath $temporaryInstaller -Destination $caseMutatedInstaller
        Assert-ThrowsLike -Pattern '*Release asset allowlist mismatch*' -Action {
            & $assetScript `
                -Directory $fixtureDirectory `
                -Version $version `
                -ReleaseTag $candidateTag `
                -Repository 'stronganchor/pusula-desktop'
        }
        Move-Item -LiteralPath $caseMutatedInstaller -Destination $temporaryInstaller
        Move-Item -LiteralPath $temporaryInstaller -Destination $canonicalInstaller

        $manifestPath = Join-Path $fixtureDirectory 'latest.json'
        $manifestCaseFixture = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
        $manifestCaseFixture.platforms.'windows-x86_64'.url =
            ([string]$manifestCaseFixture.platforms.'windows-x86_64'.url).Replace('/Pusula_', '/pusula_')
        $manifestCaseFixture | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $manifestPath -Encoding utf8
        Assert-ThrowsLike -Pattern '*Update manifest URL does not match*' -Action {
            & $assetScript `
                -Directory $fixtureDirectory `
                -Version $version `
                -ReleaseTag $candidateTag `
                -Repository 'stronganchor/pusula-desktop'
        }
    }
    finally {
        Remove-Item -LiteralPath $fixtureDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }

    $global:PusulaReleasePolicyImmutabilityEnabled = $false
    Assert-ThrowsLike -Pattern '*release immutability must be enabled*' -Action {
        & $identityScript `
            -ExpectedVersion $version `
            -Repository 'stronganchor/pusula-desktop' `
            -ExpectedCommit $commit `
            -CandidateTag $candidateTag `
            -RequireRepositoryImmutability
    }

    Assert-ThrowsLike -Pattern '*release immutability must be enabled*' -Action {
        & $promotionScript `
            -Version $version `
            -CandidateTag $candidateTag `
            -Repository 'stronganchor/pusula-desktop' `
            -WorkflowCommit $commit `
            -AcceptanceEvidenceSha256 ('a' * 64) `
            -Confirmation "PROMOTE v$version" `
            -DownloadDirectory (Join-Path $env:TEMP 'pusula-release-policy-disabled') `
            -ExpectedWindowsPublisher 'Test Publisher' `
            -MinisignPath (Join-Path $env:SystemRoot 'System32\notepad.exe')
    }

    $global:PusulaReleasePolicyImmutabilityEnabled = $true
    $global:PusulaReleasePolicyCandidateImmutable = $false
    Assert-ThrowsLike -Pattern '*Only an immutable candidate release can be promoted*' -Action {
        & $promotionScript `
            -Version $version `
            -CandidateTag $candidateTag `
            -Repository 'stronganchor/pusula-desktop' `
            -WorkflowCommit $commit `
            -AcceptanceEvidenceSha256 ('b' * 64) `
            -Confirmation "PROMOTE v$version" `
            -DownloadDirectory (Join-Path $env:TEMP 'pusula-release-policy-mutable') `
            -ExpectedWindowsPublisher 'Test Publisher' `
            -MinisignPath (Join-Path $env:SystemRoot 'System32\notepad.exe')
    }

    $global:PusulaReleasePolicyCandidateImmutable = $true
    $global:PusulaReleasePolicyStableTagExists = $true
    Assert-ThrowsLike -Pattern '*Stable tag already exists and cannot be overwritten*' -Action {
        & $promotionScript `
            -Version $version `
            -CandidateTag $candidateTag `
            -Repository 'stronganchor/pusula-desktop' `
            -WorkflowCommit $commit `
            -AcceptanceEvidenceSha256 ('c' * 64) `
            -Confirmation "PROMOTE v$version" `
            -DownloadDirectory (Join-Path $env:TEMP 'pusula-release-policy-stable') `
            -ExpectedWindowsPublisher 'Test Publisher' `
            -MinisignPath (Join-Path $env:SystemRoot 'System32\notepad.exe')
    }

    $global:PusulaReleasePolicyStableTagExists = $false
    $global:PusulaReleasePolicyStableReleaseExists = $true
    Assert-ThrowsLike -Pattern '*Stable release already exists and cannot be overwritten*' -Action {
        & $promotionScript `
            -Version $version `
            -CandidateTag $candidateTag `
            -Repository 'stronganchor/pusula-desktop' `
            -WorkflowCommit $commit `
            -AcceptanceEvidenceSha256 ('d' * 64) `
            -Confirmation "PROMOTE v$version" `
            -DownloadDirectory (Join-Path $env:TEMP 'pusula-release-policy-stable-release') `
            -ExpectedWindowsPublisher 'Test Publisher' `
            -MinisignPath (Join-Path $env:SystemRoot 'System32\notepad.exe')
    }
}
finally {
    $env:GITHUB_REF = $originalRef
    Remove-Item Function:\gh -ErrorAction SilentlyContinue
    foreach ($name in @(
            'PusulaReleasePolicyVersion',
            'PusulaReleasePolicyCommit',
            'PusulaReleasePolicyCandidateTag',
            'PusulaReleasePolicyImmutabilityEnabled',
            'PusulaReleasePolicyCandidateImmutable',
            'PusulaReleasePolicyStableTagExists',
            'PusulaReleasePolicyStableReleaseExists'
        )) {
        Remove-Variable $name -Scope Global -ErrorAction SilentlyContinue
    }
}

Write-Output 'Release policy tests passed.'
