[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path $PSScriptRoot -Parent
$identityScript = Join-Path $repoRoot 'scripts\Test-ReleaseIdentity.ps1'
$publicationScript = Join-Path $repoRoot 'scripts\Publish-VerifiedRelease.ps1'
$candidatePreparationScript = Join-Path $repoRoot 'scripts\Prepare-ResumableCandidateRelease.ps1'
$stablePreparationScript = Join-Path $repoRoot 'scripts\Prepare-ResumableStableRelease.ps1'
$repositoryControlsScript = Join-Path $repoRoot 'scripts\Release-RepositoryControls.ps1'
$candidateConfigScript = Join-Path $repoRoot 'scripts\New-CandidateUpdaterConfig.ps1'
$manifestScript = Join-Path $repoRoot 'scripts\New-UpdateManifest.ps1'
$assetScript = Join-Path $repoRoot 'scripts\Test-ReleaseAssets.ps1'
& (Join-Path $repoRoot 'tests\invalid-updater-signature-harness.tests.ps1') | Out-Host
& (Join-Path $repoRoot 'tests\release-acceptance-evidence.tests.ps1') | Out-Host
& (Join-Path $repoRoot 'tests\candidate-release-resume.tests.ps1') | Out-Host
& (Join-Path $repoRoot 'tests\release-publication.tests.ps1') | Out-Host
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
$publicationPolicy = Get-Content -Raw -LiteralPath $publicationScript
$candidatePreparationPolicy = Get-Content -Raw -LiteralPath $candidatePreparationScript
$stablePreparationPolicy = Get-Content -Raw -LiteralPath $stablePreparationScript
$repositoryControlsPolicy = Get-Content -Raw -LiteralPath $repositoryControlsScript
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
if (-not (Test-OrdinalContains -Text $releaseWorkflow -Value 'validate-gateway:') -or
    -not (Test-OrdinalContains -Text $releaseWorkflow -Value 'needs: [validate, validate-gateway]') -or
    -not (Test-OrdinalContains -Text $releaseWorkflow -Value '- name: Validate the exact gateway source shipped with this candidate') -or
    -not (Test-OrdinalContains -Text $releaseWorkflow -Value 'cargo fmt --manifest-path gateway/Cargo.toml --check') -or
    -not (Test-OrdinalContains -Text $releaseWorkflow -Value 'cargo clippy --manifest-path gateway/Cargo.toml --all-targets -- -D warnings') -or
    -not (Test-OrdinalContains -Text $releaseWorkflow -Value 'cargo test --manifest-path gateway/Cargo.toml')) {
    throw 'The signed-release workflow must validate the exact gateway commit before privileged signing.'
}

Assert-OrderedWorkflowSteps -Workflow $releaseWorkflow -StepNames @(
    'Create or safely resume the exact candidate draft',
    'Revalidate and publish the candidate draft in one write-token process'
)
Assert-OrderedWorkflowSteps -Workflow $promotionWorkflow -StepNames @(
    'Decode bounded canonical acceptance evidence',
    'Revalidate immutable candidate, signatures, and canonical evidence',
    'Add exact evidence as the sixth stable asset',
    'Create or safely resume the exact stable draft',
    'Revalidate and publish the stable draft in one write-token process'
)
if (-not (Test-OrdinalContains -Text $releaseWorkflow -Value '.\scripts\Prepare-ResumableCandidateRelease.ps1') -or
    -not (Test-OrdinalContains -Text $releaseWorkflow -Value '-AllowExactCandidateDraftResume')) {
    throw 'Candidate workflow must safely resume only its exact lightweight tag and draft.'
}
if (-not (Test-OrdinalContains -Text $promotionWorkflow -Value 'acceptance_evidence_base64:') -or
    -not (Test-OrdinalContains -Text $promotionWorkflow -Value 'actions: read') -or
    -not (Test-OrdinalContains -Text $promotionWorkflow -Value 'maximum 60 KiB encoded / 45 KiB decoded') -or
    -not (Test-OrdinalContains -Text $promotionWorkflow -Value '.\scripts\Decode-ReleaseAcceptanceEvidence.ps1') -or
    -not (Test-OrdinalContains -Text $promotionWorkflow -Value '.\scripts\Prepare-ResumableStableRelease.ps1')) {
    throw 'Stable workflow must carry bounded canonical evidence and use the safe resumable-draft helper.'
}
$candidatePublishStart = $releaseWorkflow.IndexOf('- name: Revalidate and publish the candidate draft in one write-token process', [StringComparison]::Ordinal)
$stablePublishStart = $promotionWorkflow.IndexOf('- name: Revalidate and publish the stable draft in one write-token process', [StringComparison]::Ordinal)
$candidatePublish = $releaseWorkflow.Substring($candidatePublishStart)
$stablePublish = $promotionWorkflow.Substring($stablePublishStart)
foreach ($publicationStep in @($candidatePublish, $stablePublish)) {
    if (-not (Test-OrdinalContains -Text $publicationStep -Value 'GH_TOKEN: ${{ github.token }}') -or
        -not (Test-OrdinalContains -Text $publicationStep -Value 'RELEASE_ADMIN_READ_TOKEN: ${{ secrets.RELEASE_ADMIN_READ_TOKEN }}') -or
        -not (Test-OrdinalContains -Text $publicationStep -Value '.\scripts\Publish-VerifiedRelease.ps1')) {
        throw 'Each final publication must execute the shared recheck/PATCH helper with exclusive write-token custody.'
    }
}
if (-not (Test-OrdinalContains -Text $candidatePublish -Value '-BuildInitialAcceptanceBaseline $env:BUILD_INITIAL_ACCEPTANCE_BASELINE') -or
    -not (Test-OrdinalContains -Text $candidatePublish -Value '-AcceptanceBaselineVersion $env:ACCEPTANCE_BASELINE_VERSION')) {
    throw 'Final candidate publication must recheck the exact initial baseline gate.'
}
if (-not (Test-OrdinalContains -Text $publicationPolicy -Value 'releases/assets/$assetId') -or
    -not (Test-OrdinalContains -Text $publicationPolicy -Value 'releases/$releaseId') -or
    -not (Test-OrdinalContains -Text $publicationPolicy -Value "'--method', 'PATCH'") -or
    -not (Test-OrdinalContains -Text $publicationPolicy -Value 'X-GitHub-Api-Version: 2026-03-10') -or
    -not (Test-OrdinalContains -Text $publicationPolicy -Value 'Assert-PusulaReleaseRepositoryControls') -or
    -not (Test-OrdinalContains -Text $publicationPolicy -Value 'gh release verify') -or
    (Test-OrdinalContains -Text $publicationPolicy -Value "'DELETE'") -or
    (Test-OrdinalContains -Text $publicationPolicy -Value 'release delete')) {
    throw 'Shared publication helper must revalidate numeric IDs/digests, PATCH once, verify immutability, and never delete.'
}
if (-not (Test-OrdinalContains -Text $stablePreparationPolicy -Value 'rerun will verify and resume without clobbering') -or
    -not (Test-OrdinalContains -Text $stablePreparationPolicy -Value 'never retag it') -or
    (Test-OrdinalContains -Text $stablePreparationPolicy -Value '--clobber') -or
    (Test-OrdinalContains -Text $stablePreparationPolicy -Value "'DELETE'")) {
    throw 'Stable draft preparation must resume only exact assets and never clobber, delete, or retag.'
}
if (-not (Test-OrdinalContains -Text $candidatePreparationPolicy -Value 'rerun failed jobs to verify and resume without clobbering') -or
    -not (Test-OrdinalContains -Text $candidatePreparationPolicy -Value 'never retag it') -or
    (Test-OrdinalContains -Text $candidatePreparationPolicy -Value '--clobber') -or
    (Test-OrdinalContains -Text $candidatePreparationPolicy -Value "'DELETE'")) {
    throw 'Candidate draft preparation must resume only exact assets and never clobber, delete, or retag.'
}
if (-not (Test-OrdinalContains -Text $repositoryControlsPolicy -Value "'Protect release tags'") -or
    -not (Test-OrdinalContains -Text $repositoryControlsPolicy -Value "'refs/tags/v*'") -or
    -not (Test-OrdinalContains -Text $repositoryControlsPolicy -Value "@('deletion', 'update')") -or
    -not (Test-OrdinalContains -Text $repositoryControlsPolicy -Value "current_user_can_bypass") -or
    -not (Test-OrdinalContains -Text $repositoryControlsPolicy -Value 'immutable-releases')) {
    throw 'Release repository controls must verify immutable releases and the exact no-bypass release-tag ruleset.'
}

$originalRef = $env:GITHUB_REF
$global:PusulaReleasePolicyVersion = $version
$global:PusulaReleasePolicyCommit = $commit
$global:PusulaReleasePolicyCandidateTag = $candidateTag
$global:PusulaReleasePolicyImmutabilityEnabled = $true
$global:PusulaReleasePolicyCandidateImmutable = $true
$global:PusulaReleasePolicyStableTagExists = $false
$global:PusulaReleasePolicyStableReleaseExists = $false
$global:PusulaReleasePolicyCandidateOnlyReleaseExists = $false
$global:PusulaReleasePolicyExactCandidateDraftExists = $false
$global:PusulaReleasePolicyRulesetMode = 'exact'

function global:gh {
    $global:LASTEXITCODE = 0
    $commandLine = $args -join ' '

    if ($commandLine -like '*immutable-releases*') {
        return (@{
                enabled = [bool]$global:PusulaReleasePolicyImmutabilityEnabled
                enforced_by_owner = $false
            } | ConvertTo-Json -Compress)
    }
    if ($commandLine -like '*rulesets/18968971*') {
        $bypassActors = @()
        $canBypass = 'never'
        $include = @('refs/tags/v*')
        if ($global:PusulaReleasePolicyRulesetMode -eq 'bypass') {
            $bypassActors = @(@{ actor_id = 1; actor_type = 'RepositoryRole'; bypass_mode = 'always' })
            $canBypass = 'always'
        }
        elseif ($global:PusulaReleasePolicyRulesetMode -eq 'wrong-ref') {
            $include = @('~DEFAULT_BRANCH')
        }
        return (@{
                id = 18968971
                name = 'Protect release tags'
                target = 'tag'
                enforcement = 'active'
                conditions = @{ ref_name = @{ include = $include; exclude = @() } }
                rules = @(@{ type = 'update' }, @{ type = 'deletion' })
                bypass_actors = $bypassActors
                current_user_can_bypass = $canBypass
            } | ConvertTo-Json -Depth 8 -Compress)
    }
    if ($commandLine -like '*repos/stronganchor/pusula-desktop/rulesets*') {
        return (@(@{ id = 18968971; name = 'Protect release tags'; target = 'tag' }) | ConvertTo-Json -Compress)
    }
    if ($commandLine -like '*git/ref/heads/main*') {
        return (@{ object = @{ sha = $global:PusulaReleasePolicyCommit } } | ConvertTo-Json -Compress)
    }
    if ($commandLine -like '*matching-refs/tags/*') {
        if ($global:PusulaReleasePolicyExactCandidateDraftExists) {
            return (ConvertTo-Json -InputObject @(@{
                        ref = "refs/tags/$($global:PusulaReleasePolicyCandidateTag)"
                        object = @{ sha = $global:PusulaReleasePolicyCommit; type = 'commit' }
                    }) -Depth 5 -Compress)
        }
        if ($global:PusulaReleasePolicyStableTagExists) {
            return (@(@{
                        ref = "refs/tags/v$($global:PusulaReleasePolicyVersion)"
                        object = @{ sha = $global:PusulaReleasePolicyCommit; type = 'commit' }
                    }) | ConvertTo-Json -Compress)
        }
        return '[]'
    }
    if ($commandLine -like '*releases?per_page=100*') {
        if ($global:PusulaReleasePolicyExactCandidateDraftExists) {
            return "$($global:PusulaReleasePolicyCandidateTag)`ttrue`ttrue"
        }
        if ($global:PusulaReleasePolicyStableReleaseExists) {
            return "v0.0.9`tfalse`tfalse"
        }
        if ($global:PusulaReleasePolicyCandidateOnlyReleaseExists) {
            return "v0.0.9-candidate.$('f' * 40)`tfalse`ttrue"
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
            -CandidateTag $candidateTag `
            -BuildInitialAcceptanceBaseline true `
            -AcceptanceBaselineVersion '0.0.9'
    }

    Assert-ThrowsLike -Pattern '*Candidate tag must exactly equal*' -Action {
        & $identityScript `
            -ExpectedVersion $version `
            -Repository 'stronganchor/pusula-desktop' `
            -ExpectedCommit $commit `
            -CandidateTag "v$version-candidate.$('0' * 40)" `
            -BuildInitialAcceptanceBaseline true `
            -AcceptanceBaselineVersion '0.0.9'
    }

    Assert-ThrowsLike -Pattern '*first stable release requires build_initial_acceptance_baseline=true*' -Action {
        & $identityScript `
            -ExpectedVersion $version -Repository 'stronganchor/pusula-desktop' -ExpectedCommit $commit `
            -CandidateTag $candidateTag -BuildInitialAcceptanceBaseline false -AcceptanceBaselineVersion '0.0.9'
    }
    Assert-ThrowsLike -Pattern '*exact 0.0.9 acceptance baseline*' -Action {
        & $identityScript `
            -ExpectedVersion $version -Repository 'stronganchor/pusula-desktop' -ExpectedCommit $commit `
            -CandidateTag $candidateTag -BuildInitialAcceptanceBaseline true -AcceptanceBaselineVersion '0.0.8'
    }
    & $identityScript `
        -ExpectedVersion $version `
        -Repository 'stronganchor/pusula-desktop' `
        -ExpectedCommit $commit `
        -CandidateTag $candidateTag `
        -BuildInitialAcceptanceBaseline true `
        -AcceptanceBaselineVersion '0.0.9' | Out-Host

    $global:PusulaReleasePolicyStableReleaseExists = $true
    & $identityScript `
        -ExpectedVersion $version -Repository 'stronganchor/pusula-desktop' -ExpectedCommit $commit `
        -CandidateTag $candidateTag -BuildInitialAcceptanceBaseline false -AcceptanceBaselineVersion '0.0.9' | Out-Host
    $global:PusulaReleasePolicyStableReleaseExists = $false
    $global:PusulaReleasePolicyCandidateOnlyReleaseExists = $true
    Assert-ThrowsLike -Pattern '*first stable release requires build_initial_acceptance_baseline=true*' -Action {
        & $identityScript `
            -ExpectedVersion $version -Repository 'stronganchor/pusula-desktop' -ExpectedCommit $commit `
            -CandidateTag $candidateTag -BuildInitialAcceptanceBaseline false -AcceptanceBaselineVersion '0.0.9'
    }
    $global:PusulaReleasePolicyCandidateOnlyReleaseExists = $false
    $global:PusulaReleasePolicyExactCandidateDraftExists = $true
    & $identityScript `
        -ExpectedVersion $version -Repository 'stronganchor/pusula-desktop' -ExpectedCommit $commit `
        -CandidateTag $candidateTag -BuildInitialAcceptanceBaseline true -AcceptanceBaselineVersion '0.0.9' `
        -AllowExactCandidateDraftResume | Out-Host
    Assert-ThrowsLike -Pattern '*already exists for version*' -Action {
        & $identityScript `
            -ExpectedVersion $version -Repository 'stronganchor/pusula-desktop' -ExpectedCommit $commit `
            -CandidateTag $candidateTag -BuildInitialAcceptanceBaseline true -AcceptanceBaselineVersion '0.0.9'
    }
    $global:PusulaReleasePolicyExactCandidateDraftExists = $false

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
            -BuildInitialAcceptanceBaseline true `
            -AcceptanceBaselineVersion '0.0.9' `
            -RequireRepositoryImmutability
    }

    $global:PusulaReleasePolicyImmutabilityEnabled = $true
    $global:PusulaReleasePolicyRulesetMode = 'bypass'
    Assert-ThrowsLike -Pattern '*no bypass actors*' -Action {
        & $identityScript `
            -ExpectedVersion $version -Repository 'stronganchor/pusula-desktop' -ExpectedCommit $commit `
            -CandidateTag $candidateTag -BuildInitialAcceptanceBaseline true -AcceptanceBaselineVersion '0.0.9' `
            -RequireRepositoryImmutability
    }
    $global:PusulaReleasePolicyRulesetMode = 'wrong-ref'
    Assert-ThrowsLike -Pattern '*include only refs/tags/v*' -Action {
        & $identityScript `
            -ExpectedVersion $version -Repository 'stronganchor/pusula-desktop' -ExpectedCommit $commit `
            -CandidateTag $candidateTag -BuildInitialAcceptanceBaseline true -AcceptanceBaselineVersion '0.0.9' `
            -RequireRepositoryImmutability
    }
    $global:PusulaReleasePolicyRulesetMode = 'exact'
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
            'PusulaReleasePolicyStableReleaseExists',
            'PusulaReleasePolicyCandidateOnlyReleaseExists',
            'PusulaReleasePolicyExactCandidateDraftExists',
            'PusulaReleasePolicyRulesetMode'
        )) {
        Remove-Variable $name -Scope Global -ErrorAction SilentlyContinue
    }
}

Write-Output 'Release policy tests passed.'
