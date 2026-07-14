[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path $PSScriptRoot -Parent
$harnessScript = Join-Path $repoRoot 'scripts\Test-InvalidTauriUpdaterAcceptance.ps1'
$runtimeHelper = Join-Path $repoRoot 'scripts\Invoke-InvalidUpdaterRuntime.mjs'
$fixtureRoot = Join-Path $env:TEMP ('pusula-invalid-updater-test-' + [Guid]::NewGuid().ToString('N'))

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

try {
    $node = (Get-Command node -ErrorAction Stop).Source
    & $node --check $runtimeHelper
    if ($LASTEXITCODE -ne 0) { throw 'Invalid updater runtime helper did not pass Node syntax checking.' }

    [IO.Directory]::CreateDirectory($fixtureRoot) | Out-Null
    $artifactPath = Join-Path $fixtureRoot 'Pusula_0.1.0_x64-setup.exe'
    $signaturePath = "$artifactPath.sig"
    $configPath = Join-Path $fixtureRoot 'tauri.conf.json'
    $fakeMinisignPath = Join-Path $fixtureRoot 'fake-minisign.cmd'
    $fakeMinisignMarker = Join-Path $fixtureRoot 'fake-minisign-called'
    $outputPath = Join-Path $fixtureRoot 'prepared'

    $bytes = [byte[]]::new(8192)
    for ($index = 0; $index -lt $bytes.Length; $index += 1) {
        $bytes[$index] = [byte]($index % 251)
    }
    [IO.File]::WriteAllBytes($artifactPath, $bytes)
    $candidateHashBefore = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifactPath).Hash.ToLowerInvariant()

    $publicDocument = "untrusted comment: fixture public key`nRWQfixture-public-key`n"
    $signatureDocument = "untrusted comment: fixture signature`nRWQfixture-signature`ntrusted comment: fixture`nfixture`n"
    [IO.File]::WriteAllText($signaturePath, [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($signatureDocument)))
    $config = [ordered]@{
        plugins = [ordered]@{
            updater = [ordered]@{
                pubkey = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($publicDocument))
                endpoints = @('https://example.invalid/latest.json')
            }
        }
    }
    [IO.File]::WriteAllText(
        $configPath,
        ($config | ConvertTo-Json -Depth 8),
        [Text.UTF8Encoding]::new($false)
    )

    $fakeMinisign = @"
@echo off
if exist "$fakeMinisignMarker" exit /b 1
type nul > "$fakeMinisignMarker"
exit /b 0
"@
    [IO.File]::WriteAllText($fakeMinisignPath, $fakeMinisign, [Text.ASCIIEncoding]::new())

    & $harnessScript `
        -ArtifactPath $artifactPath `
        -SignaturePath $signaturePath `
        -CandidateVersion '0.1.0' `
        -HarnessVersion '0.0.9' `
        -MinisignPath $fakeMinisignPath `
        -OutputDirectory $outputPath `
        -TauriConfigPath $configPath `
        -Port 43191 `
        -DebugPort 43192 `
        -PreparationOnly | Out-Host

    $candidateHashAfter = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifactPath).Hash.ToLowerInvariant()
    if ($candidateHashAfter -cne $candidateHashBefore) {
        throw 'Preparation test changed the source candidate fixture.'
    }
    if (Test-Path -LiteralPath (Join-Path $outputPath 'loopback-fixture\Pusula_0.1.0_x64-setup.exe')) {
        throw 'Preparation-only mode retained the deliberately invalid executable.'
    }

    $override = Get-Content -Raw -LiteralPath (Join-Path $outputPath 'isolated-tauri-config.json') | ConvertFrom-Json
    if ([string]$override.identifier -cnotmatch '^com\.stronganchor\.pusula\.invalid-signature-test\.[0-9a-f]{32}$') {
        throw 'Harness override did not use a unique isolated application identifier.'
    }
    if ([string]$override.plugins.updater.endpoints[0] -cne 'http://127.0.0.1:43191/latest.json') {
        throw 'Harness override is not pinned to its loopback fixture.'
    }
    if ($null -ne $override.plugins.updater.PSObject.Properties['pubkey']) {
        throw 'Harness override must inherit, not replace, the production updater public key.'
    }
    foreach ($dangerousName in @(
            'dangerousInsecureTransportProtocol',
            'dangerousAcceptInvalidCerts',
            'dangerousAcceptInvalidHostnames'
        )) {
        if ($null -ne $override.plugins.updater.PSObject.Properties[$dangerousName]) {
            throw "Harness override must not write $dangerousName."
        }
    }

    $evidence = Get-Content -Raw -LiteralPath (Join-Path $outputPath 'invalid-signature-evidence.json') | ConvertFrom-Json
    if ([string]$evidence.result -cne 'preparation-only' -or [bool]$evidence.runtime_executed) {
        throw 'Preparation-only evidence must not claim a runtime acceptance pass.'
    }
    if (-not [bool]$evidence.candidate_unchanged -or
        -not [bool]$evidence.signature_unchanged -or
        [string]$evidence.original_signature_verification -cne 'accepted' -or
        [string]$evidence.tampered_copy_signature_verification -cne 'rejected') {
        throw 'Preparation evidence did not record the expected cryptographic controls.'
    }
    if ([bool]$evidence.dangerous_updater_overrides -or
        [bool]$evidence.production_configuration_modified -or
        [bool]$evidence.installer_created_or_run) {
        throw 'Preparation evidence reports an unsafe action.'
    }

    $insideRepoOutput = Join-Path $repoRoot ('invalid-signature-output-' + [Guid]::NewGuid().ToString('N'))
    Assert-ThrowsLike -Pattern '*output must be outside the repository*' -Action {
        & $harnessScript `
            -ArtifactPath $artifactPath `
            -SignaturePath $signaturePath `
            -CandidateVersion '0.1.0' `
            -HarnessVersion '0.0.9' `
            -MinisignPath $fakeMinisignPath `
            -OutputDirectory $insideRepoOutput `
            -TauriConfigPath $configPath `
            -PreparationOnly
    }
    if (Test-Path -LiteralPath $insideRepoOutput) {
        throw 'Rejected in-repository output path was created.'
    }

    Assert-ThrowsLike -Pattern '*harness version must be lower than the candidate*' -Action {
        & $harnessScript `
            -ArtifactPath $artifactPath `
            -SignaturePath $signaturePath `
            -CandidateVersion '0.1.0' `
            -HarnessVersion '0.1.0' `
            -MinisignPath $fakeMinisignPath `
            -OutputDirectory (Join-Path $fixtureRoot 'bad-version') `
            -TauriConfigPath $configPath `
            -PreparationOnly
    }

    Assert-ThrowsLike -Pattern '*must inherit the repository production Tauri public key and configuration*' -Action {
        & $harnessScript `
            -ArtifactPath $artifactPath `
            -SignaturePath $signaturePath `
            -CandidateVersion '0.1.0' `
            -HarnessVersion '0.0.9' `
            -MinisignPath $fakeMinisignPath `
            -OutputDirectory (Join-Path $fixtureRoot 'custom-runtime-config') `
            -TauriConfigPath $configPath
    }
}
finally {
    Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output 'Invalid updater signature harness tests passed.'
