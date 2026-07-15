[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path $PSScriptRoot -Parent
$commonScript = Join-Path $repoRoot 'scripts\Release-AcceptanceEvidence.ps1'
$producerScript = Join-Path $repoRoot 'scripts\New-ReleaseAcceptanceEvidence.ps1'
$decoderScript = Join-Path $repoRoot 'scripts\Decode-ReleaseAcceptanceEvidence.ps1'
$verifierScript = Join-Path $repoRoot 'scripts\Test-ReleaseAcceptanceEvidence.ps1'
$fixturePath = Join-Path $repoRoot 'tests\fixtures\pusula-lite-v1.json'
. $commonScript

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

function Copy-JsonValue {
    param($Value)
    return ($Value | ConvertTo-Json -Depth 20 -Compress) | ConvertFrom-Json
}

function Write-Utf8NoBom {
    param([string] $Path, [string] $Text)
    [IO.File]::WriteAllText($Path, $Text, [Text.UTF8Encoding]::new($false))
}

function Get-LowerSha256 {
    param([string] $Path)
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

$tempRoot = Join-Path $env:TEMP ('pusula-acceptance-evidence-tests-' + [Guid]::NewGuid().ToString('N'))
$assetDirectory = Join-Path $tempRoot 'candidate-assets'
$version = '0.1.0'
$repository = 'stronganchor/pusula-desktop'
$commit = 'a' * 40
$candidateTag = "v$version-candidate.$commit"
$workflowRunId = 123456789L
$global:PusulaEvidenceRunCommit = $commit

function global:gh {
    $global:LASTEXITCODE = 0
    $commandLine = $args -join ' '
    if ($commandLine -like "*actions/runs/$workflowRunId*") {
        return (@{
                id = $workflowRunId
                event = 'workflow_dispatch'
                status = 'completed'
                conclusion = 'success'
                head_branch = 'main'
                head_sha = $global:PusulaEvidenceRunCommit
                path = '.github/workflows/release.yml'
                repository = @{ full_name = $repository }
            } | ConvertTo-Json -Compress)
    }
    throw "Unexpected mocked gh invocation: $commandLine"
}

try {
    [IO.Directory]::CreateDirectory($assetDirectory) | Out-Null
    $assetContents = [ordered]@{
        "Pusula_${version}_x64_offline-setup.exe" = 'offline-installer-bytes'
        "Pusula_${version}_x64-setup.exe" = 'lean-installer-bytes'
        "Pusula_${version}_x64-setup.exe.sig" = 'detached-signature-bytes'
        'latest.json' = '{"version":"0.1.0"}'
        'SHA256SUMS.txt' = 'fixture sums'
    }
    foreach ($entry in $assetContents.GetEnumerator()) {
        Write-Utf8NoBom -Path (Join-Path $assetDirectory $entry.Key) -Text $entry.Value
    }
    $assetRows = @(Get-ChildItem -LiteralPath $assetDirectory -File | ForEach-Object {
            [ordered]@{
                name = $_.Name
                size = [long]$_.Length
                sha256 = Get-LowerSha256 $_.FullName
            }
        })
    $leanHash = Get-LowerSha256 (Join-Path $assetDirectory "Pusula_${version}_x64-setup.exe")
    $signatureHash = Get-LowerSha256 (Join-Path $assetDirectory "Pusula_${version}_x64-setup.exe.sig")
    $summary = [ordered]@{
        counts = [ordered]@{ customers = 2; contacts = 1; sales = 2; installments = 1; payments = 1 }
        totals = [ordered]@{ sales_kurus = 1234567900L; installments_kurus = 5; payments_kurus = 1 }
    }
    $checks = [ordered]@{}
    foreach ($name in $script:PusulaAcceptanceCheckNames) { $checks[$name] = 'pass' }
    $baseEvidence = [pscustomobject][ordered]@{
        schema_version = 3
        repository = $repository
        version = $version
        candidate = [ordered]@{
            tag = $candidateTag
            commit = $commit
            workflow_run_id = $workflowRunId
            assets = $assetRows
        }
        acceptance = [ordered]@{
            started_at_utc = '2026-01-01T00:00:00.0000000Z'
            completed_at_utc = '2026-01-01T01:00:00.0000000Z'
            windows = [ordered]@{
                version = '10.0.26100.1'
                architecture = 'windows-x86_64'
                standard_user = $true
                clean_profile = $true
                network_disconnected = $true
                offline_install = $true
                restart_completed = $true
            }
            windows_distribution = [ordered]@{
                mode = 'managed_unsigned_single_machine'
                install_mode = 'currentUser'
                offline_installer_authenticode_status = 'NotSigned'
                updater_installer_authenticode_status = 'NotSigned'
                initial_installer_sha256_verified = $true
                initial_smartscreen_acknowledged = $true
                trusted_publisher_certificate_installed = $false
                tauri_updater_signature_verified = $true
                in_app_update_manual_prompts = 1
            }
            baseline = [ordered]@{
                version = '0.0.9'
                archive_sha256 = 'c' * 64
                installed_exe_sha256 = 'd' * 64
            }
            candidate_install = [ordered]@{
                version = $version
                installed_exe_sha256 = 'e' * 64
            }
            fixture_restore = [ordered]@{
                fixture_manifest_sha256 = 'd709a52df5147bddd57d569d1de4113f76ac10f8841405d970e4e60bdd90ade6'
                source = $summary
                restored = $summary
            }
            backup = [ordered]@{
                backup_id = '12345678-1234-4123-8123-123456789abc'
                ciphertext_sha256 = 'f' * 64
                desktop_size = 987654L
                gateway_sha256 = 'f' * 64
                gateway_size = 987654L
                gateway_version_id = "fs-sha256-$('f' * 64)"
                gateway_verified_at_utc = '2026-01-01T00:30:00.0000000Z'
                storage_sha256 = 'f' * 64
                storage_size = 987654L
                storage_version_id = "fs-sha256-$('f' * 64)"
                gateway_spool_empty = $true
                sqlite_integrity = 'ok'
                foreign_keys = 'ok'
            }
            invalid_signature = [ordered]@{
                evidence_sha256 = '1' * 64
                result = 'pass'
                source_commit = $commit
                candidate_sha256 = $leanHash
                signature_sha256 = $signatureHash
                candidate_unchanged = $true
                signature_unchanged = $true
                original_signature_verification = 'accepted'
                tampered_copy_signature_verification = 'rejected'
                runtime_rejection_phase = 'downloading'
                installation_confirmation_called = $false
                dangerous_updater_overrides = $false
                production_configuration_modified = $false
                installer_created_or_run = $false
            }
            checks = $checks
        }
    }

    $inputPath = Join-Path $tempRoot 'evidence-input.json'
    $canonicalPath = Join-Path $tempRoot 'evidence-canonical.json'
    Write-Utf8NoBom -Path $inputPath -Text ($baseEvidence | ConvertTo-Json -Depth 20)
    $producerOutput = @(& $producerScript `
        -InputPath $inputPath `
        -OutputPath $canonicalPath `
        -CandidateAssetDirectory $assetDirectory `
        -Repository $repository `
        -Version $version `
        -CandidateTag $candidateTag `
        -CandidateCommit $commit)
    @($producerOutput | Where-Object { $_ -notlike 'workflow_dispatch base64:*' }) | Out-Host
    $canonicalHash = Get-LowerSha256 $canonicalPath

    & $verifierScript `
        -EvidencePath $canonicalPath `
        -ExpectedSha256 $canonicalHash `
        -CandidateAssetDirectory $assetDirectory `
        -Repository $repository `
        -Version $version `
        -CandidateTag $candidateTag `
        -CandidateCommit $commit `
        -ActionsToken 'test-actions-token' | Out-Host

    $canonicalBytes = [IO.File]::ReadAllBytes($canonicalPath)
    $canonicalBase64 = [Convert]::ToBase64String($canonicalBytes)
    $decodedPath = Join-Path $tempRoot 'decoded.json'
    & $decoderScript -Base64 $canonicalBase64 -ExpectedSha256 $canonicalHash -OutputPath $decodedPath | Out-Host
    if (-not [Linq.Enumerable]::SequenceEqual([byte[]]$canonicalBytes, [byte[]][IO.File]::ReadAllBytes($decodedPath))) {
        throw 'Strict base64 decoder did not preserve canonical evidence bytes.'
    }

    Assert-ThrowsLike -Pattern '*strict standard base64*' -Action {
        $null = ConvertFrom-PusulaAcceptanceEvidenceBase64 -Base64 ($canonicalBase64 + "`n")
    }
    Assert-ThrowsLike -Pattern '*exact canonical encoding*' -Action {
        $null = ConvertFrom-PusulaAcceptanceEvidenceBase64 -Base64 'Zh=='
    }
    Assert-ThrowsLike -Pattern '*61440 characters*' -Action {
        $null = ConvertFrom-PusulaAcceptanceEvidenceBase64 -Base64 ('A' * 61444)
    }
    Assert-ThrowsLike -Pattern '*without a BOM*' -Action {
        $null = ConvertFrom-PusulaAcceptanceEvidenceBase64 -Base64 ([Convert]::ToBase64String([byte[]](0xEF, 0xBB, 0xBF, 0x7B, 0x7D)))
    }
    Assert-ThrowsLike -Pattern '*valid strict UTF-8*' -Action {
        $null = ConvertFrom-PusulaAcceptanceEvidenceBase64 -Base64 ([Convert]::ToBase64String([byte[]](0xC3, 0x28)))
    }
    Assert-ThrowsLike -Pattern '*does not match the supplied promotion digest*' -Action {
        & $decoderScript -Base64 $canonicalBase64 -ExpectedSha256 ('9' * 64) -OutputPath (Join-Path $tempRoot 'wrong-digest.json')
    }

    function Invoke-MutatedEvidence {
        param($Value, [string] $ExpectedError)
        $path = Join-Path $tempRoot ('mutation-' + [Guid]::NewGuid().ToString('N') + '.json')
        Write-Utf8NoBom -Path $path -Text ($Value | ConvertTo-Json -Depth 20 -Compress)
        $hash = Get-LowerSha256 $path
        Assert-ThrowsLike -Pattern $ExpectedError -Action {
            & $verifierScript `
                -EvidencePath $path -ExpectedSha256 $hash -CandidateAssetDirectory $assetDirectory `
                -Repository $repository -Version $version -CandidateTag $candidateTag -CandidateCommit $commit `
                -ActionsToken 'test-actions-token'
        }
    }

    $unknown = Copy-JsonValue $baseEvidence
    $unknown | Add-Member -NotePropertyName notes -NotePropertyValue 'forbidden free text'
    Invoke-MutatedEvidence $unknown '*properties must exactly equal*'

    $failedCheck = Copy-JsonValue $baseEvidence
    $failedCheck.acceptance.checks.restore = 'fail'
    Invoke-MutatedEvidence $failedCheck '*checks.restore must equal pass*'

    $stringInteger = Copy-JsonValue $baseEvidence
    $stringInteger.candidate.workflow_run_id = [string]$workflowRunId
    Invoke-MutatedEvidence $stringInteger '*workflow_run_id must be an exact JSON integer*'

    $wrongAsset = Copy-JsonValue $baseEvidence
    $wrongAsset.candidate.assets[0].sha256 = '2' * 64
    Invoke-MutatedEvidence $wrongAsset '*asset bytes differ*'

    $wrongDistribution = Copy-JsonValue $baseEvidence
    $wrongDistribution.acceptance.windows_distribution.mode = 'public_signed_distribution'
    Invoke-MutatedEvidence $wrongDistribution '*managed unsigned current-user release mode*'

    $signedInstaller = Copy-JsonValue $baseEvidence
    $signedInstaller.acceptance.windows_distribution.updater_installer_authenticode_status = 'Valid'
    Invoke-MutatedEvidence $signedInstaller '*managed unsigned current-user release mode*'

    $missingSmartScreenAcknowledgement = Copy-JsonValue $baseEvidence
    $missingSmartScreenAcknowledgement.acceptance.windows_distribution.initial_smartscreen_acknowledged = $false
    Invoke-MutatedEvidence $missingSmartScreenAcknowledgement '*initial_smartscreen_acknowledged must be the JSON boolean true*'

    $trustedSelfSignedCertificate = Copy-JsonValue $baseEvidence
    $trustedSelfSignedCertificate.acceptance.windows_distribution.trusted_publisher_certificate_installed = $true
    Invoke-MutatedEvidence $trustedSelfSignedCertificate '*trusted_publisher_certificate_installed must be the JSON boolean false*'

    $missingUpdateConfirmation = Copy-JsonValue $baseEvidence
    $missingUpdateConfirmation.acceptance.windows_distribution.in_app_update_manual_prompts = 0
    Invoke-MutatedEvidence $missingUpdateConfirmation '*in_app_update_manual_prompts must equal 1*'

    $extraUpdatePrompts = Copy-JsonValue $baseEvidence
    $extraUpdatePrompts.acceptance.windows_distribution.in_app_update_manual_prompts = 2
    Invoke-MutatedEvidence $extraUpdatePrompts '*in_app_update_manual_prompts must equal 1*'

    $wrongBaseline = Copy-JsonValue $baseEvidence
    $wrongBaseline.acceptance.baseline.version = '0.0.8'
    Invoke-MutatedEvidence $wrongBaseline '*baseline.version must equal 0.0.9*'

    $wrongFixture = Copy-JsonValue $baseEvidence
    $wrongFixture.acceptance.fixture_restore.fixture_manifest_sha256 = '3' * 64
    Invoke-MutatedEvidence $wrongFixture '*logical Pusula fixture manifest*'

    $wrongRestore = Copy-JsonValue $baseEvidence
    $wrongRestore.acceptance.fixture_restore.restored.counts.sales = 3
    Invoke-MutatedEvidence $wrongRestore '*restored.counts.sales does not match*'

    $wrongBackup = Copy-JsonValue $baseEvidence
    $wrongBackup.acceptance.backup.storage_size = 987655
    Invoke-MutatedEvidence $wrongBackup '*ciphertext size/hash evidence must exactly match*'

    $wrongVersion = Copy-JsonValue $baseEvidence
    $wrongVersion.acceptance.backup.storage_version_id = 'different-version'
    Invoke-MutatedEvidence $wrongVersion '*version IDs must equal the deterministic*'

    $controlVersion = Copy-JsonValue $baseEvidence
    $controlVersion.acceptance.backup.gateway_version_id = "version`nwith-control"
    $controlVersion.acceptance.backup.storage_version_id = "version`nwith-control"
    Invoke-MutatedEvidence $controlVersion '*must not contain control characters*'

    $longVersion = Copy-JsonValue $baseEvidence
    $longVersion.acceptance.backup.gateway_version_id = 'v' * 257
    $longVersion.acceptance.backup.storage_version_id = 'v' * 257
    Invoke-MutatedEvidence $longVersion '*must contain at most 256 characters*'

    $nonCanonicalVerification = Copy-JsonValue $baseEvidence
    $nonCanonicalVerification.acceptance.backup.gateway_verified_at_utc = '2026-01-01T00:30:00Z'
    Invoke-MutatedEvidence $nonCanonicalVerification '*gateway_verified_at_utc has an invalid format*'

    $lateVerification = Copy-JsonValue $baseEvidence
    $lateVerification.acceptance.backup.gateway_verified_at_utc = '2026-01-01T01:00:00.0000001Z'
    Invoke-MutatedEvidence $lateVerification '*must fall within the completed acceptance interval*'

    $spooledBackup = Copy-JsonValue $baseEvidence
    $spooledBackup.acceptance.backup.gateway_spool_empty = $false
    Invoke-MutatedEvidence $spooledBackup '*gateway_spool_empty must be the JSON boolean true*'

    $wrongInvalidSignature = Copy-JsonValue $baseEvidence
    $wrongInvalidSignature.acceptance.invalid_signature.source_commit = '4' * 40
    Invoke-MutatedEvidence $wrongInvalidSignature '*not a pass from the candidate source commit*'

    $prettyPath = Join-Path $tempRoot 'pretty.json'
    Write-Utf8NoBom -Path $prettyPath -Text ($baseEvidence | ConvertTo-Json -Depth 20)
    $prettyHash = Get-LowerSha256 $prettyPath
    Assert-ThrowsLike -Pattern '*not the exact canonical compact JSON*' -Action {
        & $verifierScript `
            -EvidencePath $prettyPath -ExpectedSha256 $prettyHash -CandidateAssetDirectory $assetDirectory `
            -Repository $repository -Version $version -CandidateTag $candidateTag -CandidateCommit $commit `
            -ActionsToken 'test-actions-token'
    }

    $duplicatePath = Join-Path $tempRoot 'duplicate.json'
    $duplicateText = ([IO.File]::ReadAllText($canonicalPath)).Replace(
        '{"schema_version":3,',
        '{"schema_version":3,"schema_version":3,'
    )
    Write-Utf8NoBom -Path $duplicatePath -Text $duplicateText
    Assert-ThrowsLike -Pattern '*Duplicate JSON property is forbidden*' -Action {
        & $verifierScript `
            -EvidencePath $duplicatePath -ExpectedSha256 (Get-LowerSha256 $duplicatePath) `
            -CandidateAssetDirectory $assetDirectory -Repository $repository -Version $version `
            -CandidateTag $candidateTag -CandidateCommit $commit -ActionsToken 'test-actions-token'
    }

    $oversizedPath = Join-Path $tempRoot 'oversized.json'
    Write-Utf8NoBom -Path $oversizedPath -Text (' ' * 46081)
    Assert-ThrowsLike -Pattern '*between 1 and 46080 bytes*' -Action {
        $null = Read-PusulaStrictUtf8File -Path $oversizedPath
    }

    $global:PusulaEvidenceRunCommit = '5' * 40
    Assert-ThrowsLike -Pattern '*workflow run is not the successful candidate release run*' -Action {
        & $verifierScript `
            -EvidencePath $canonicalPath -ExpectedSha256 $canonicalHash -CandidateAssetDirectory $assetDirectory `
            -Repository $repository -Version $version -CandidateTag $candidateTag -CandidateCommit $commit `
            -ActionsToken 'test-actions-token'
    }
    $global:PusulaEvidenceRunCommit = $commit

    $fixtureText = Get-Content -Raw -LiteralPath $fixturePath
    $normalizedFixture = $fixtureText.Replace("`r`n", "`n").Replace("`r", "`n")
    $lfFixture = Join-Path $tempRoot 'fixture-lf.json'
    $crlfFixture = Join-Path $tempRoot 'fixture-crlf.json'
    Write-Utf8NoBom -Path $lfFixture -Text $normalizedFixture
    Write-Utf8NoBom -Path $crlfFixture -Text $normalizedFixture.Replace("`n", "`r`n")
    $lfCanonical = Get-PusulaCanonicalAcceptanceEvidence `
        -Evidence (Copy-JsonValue $baseEvidence) -Repository $repository -Version $version `
        -CandidateTag $candidateTag -CandidateCommit $commit -CandidateAssetDirectory $assetDirectory `
        -FixturePath $lfFixture
    $crlfCanonical = Get-PusulaCanonicalAcceptanceEvidence `
        -Evidence (Copy-JsonValue $baseEvidence) -Repository $repository -Version $version `
        -CandidateTag $candidateTag -CandidateCommit $commit -CandidateAssetDirectory $assetDirectory `
        -FixturePath $crlfFixture
    if ((ConvertTo-PusulaCanonicalJson $lfCanonical) -cne (ConvertTo-PusulaCanonicalJson $crlfCanonical)) {
        throw 'Logical fixture evidence changed between LF and CRLF representations.'
    }
}
finally {
    Remove-Item Function:\gh -ErrorAction SilentlyContinue
    Remove-Variable PusulaEvidenceRunCommit -Scope Global -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output 'Release acceptance evidence tests passed.'
