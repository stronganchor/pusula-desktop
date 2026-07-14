[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path $PSScriptRoot -Parent
$harnessScript = Join-Path $repoRoot 'scripts\Test-InvalidTauriUpdaterAcceptance.ps1'
$runtimeHelper = Join-Path $repoRoot 'scripts\Invoke-InvalidUpdaterRuntime.mjs'
$fixtureRoot = Join-Path $env:TEMP ('pusula-invalid-updater-test-' + [Guid]::NewGuid().ToString('N'))
$sourceCommit = (& git -C $repoRoot rev-parse --verify HEAD).Trim().ToLowerInvariant()
if ($LASTEXITCODE -ne 0 -or $sourceCommit -cnotmatch '^[0-9a-f]{40}$') {
    throw 'Could not determine the exact repository source commit for harness tests.'
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

try {
    $node = (Get-Command node -ErrorAction Stop).Source
    & $node --check $runtimeHelper
    if ($LASTEXITCODE -ne 0) { throw 'Invalid updater runtime helper did not pass Node syntax checking.' }

    $tokens = $null
    $parseErrors = $null
    $harnessAst = [Management.Automation.Language.Parser]::ParseFile(
        $harnessScript,
        [ref]$tokens,
        [ref]$parseErrors
    )
    if ($parseErrors.Count -ne 0) { throw 'Invalid updater harness did not parse as PowerShell.' }
    $harnessText = Get-Content -Raw -LiteralPath $harnessScript
    $cleanupIndex = $harnessText.LastIndexOf("finally {", [StringComparison]::Ordinal)
    $passEvidenceWriteIndex = $harnessText.IndexOf(
        'Write-JsonFile -Value $evidence -Path $evidencePath',
        [StringComparison]::Ordinal
    )
    if ($cleanupIndex -lt 0 -or $passEvidenceWriteIndex -le $cleanupIndex) {
        throw 'Runtime pass evidence must be written only after fail-closed cleanup completes.'
    }
    foreach ($functionName in @(
            'Stop-ExactProcess',
            'Wait-LoopbackPortClosed',
            'Start-ArgumentProcess',
            'Invoke-GitRead',
            'Invoke-GitQuietDiff',
            'Resolve-RequiredExecutable',
            'Assert-ExpectedCleanSource'
        )) {
        $functionAst = $harnessAst.Find({
                param($node)
                $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
                $node.Name -ceq $functionName
            }, $true)
        if ($null -eq $functionAst) { throw "Harness function is missing: $functionName" }
        . ([ScriptBlock]::Create($functionAst.Extent.Text))
    }

    $sleepProcess = Start-Process -FilePath 'powershell.exe' `
        -ArgumentList @('-NoProfile', '-Command', 'Start-Sleep -Seconds 30') `
        -WindowStyle Hidden `
        -PassThru
    try {
        $stopProblem = Stop-ExactProcess -Process $sleepProcess -Label 'test child'
        if ($stopProblem) { throw "Exact-process cleanup unexpectedly failed: $stopProblem" }
        $sleepProcess.Refresh()
        if (-not $sleepProcess.HasExited) { throw 'Exact-process cleanup returned success for a live process.' }
    }
    finally {
        if (-not $sleepProcess.HasExited) { Stop-Process -Id $sleepProcess.Id -Force -ErrorAction SilentlyContinue }
        $sleepProcess.Dispose()
    }

    $disposedProcess = Start-Process -FilePath 'powershell.exe' `
        -ArgumentList @('-NoProfile', '-Command', 'Start-Sleep -Seconds 30') `
        -WindowStyle Hidden `
        -PassThru
    $disposedProcessId = $disposedProcess.Id
    try {
        $disposedProcess.Dispose()
        $stopProblem = Stop-ExactProcess -Process $disposedProcess -Label 'disposed test child'
        if ([string]::IsNullOrWhiteSpace([string]$stopProblem)) {
            throw 'Exact-process cleanup suppressed a process-inspection failure.'
        }
    }
    finally {
        Stop-Process -Id $disposedProcessId -Force -ErrorAction SilentlyContinue
    }

    $portListener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $portListener.Start()
    $occupiedPort = ([Net.IPEndPoint]$portListener.LocalEndpoint).Port
    try {
        if (-not (Wait-LoopbackPortClosed -Port $occupiedPort -AttemptCount 1 -DelayMilliseconds 1)) {
            throw 'Loopback-port cleanup check returned success while the port was listening.'
        }
    }
    finally {
        $portListener.Stop()
    }
    if (Wait-LoopbackPortClosed -Port $occupiedPort -AttemptCount 2 -DelayMilliseconds 1) {
        throw 'Loopback-port cleanup check did not accept a closed port.'
    }

    $noArgumentProcess = Start-ArgumentProcess `
        -FilePath (Get-Command 'whoami.exe' -ErrorAction Stop).Source `
        -ArgumentList @() `
        -WorkingDirectory $repoRoot
    try {
        if (-not $noArgumentProcess.WaitForExit(10000)) {
            throw 'Zero-argument child-process regression fixture did not exit.'
        }
        if ($noArgumentProcess.ExitCode -ne 0) {
            throw "Zero-argument child-process regression fixture exited $($noArgumentProcess.ExitCode)."
        }
    }
    finally {
        $stopProblem = Stop-ExactProcess -Process $noArgumentProcess -Label 'zero-argument test child'
        if ($stopProblem) { throw "Zero-argument child cleanup failed: $stopProblem" }
        $noArgumentProcess.Dispose()
    }

    $preferredCargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (Test-Path -LiteralPath $preferredCargo -PathType Leaf) {
        $resolvedCargo = Resolve-RequiredExecutable -Name 'cargo.exe' -PreferredPath $preferredCargo
        if ($resolvedCargo -cne (Resolve-Path -LiteralPath $preferredCargo).Path) {
            throw 'Rust toolchain resolution did not select the exact preferred Cargo executable.'
        }
    }

    $sourceGuardRoot = Join-Path $fixtureRoot 'source-guard-repository'
    [IO.Directory]::CreateDirectory($sourceGuardRoot) | Out-Null
    & git -C $sourceGuardRoot init --quiet
    & git -C $sourceGuardRoot config user.name 'Pusula Harness Test'
    & git -C $sourceGuardRoot config user.email 'harness-test@invalid.example'
    $guardFile = Join-Path $sourceGuardRoot 'source.txt'
    [IO.File]::WriteAllText($guardFile, 'committed source', [Text.UTF8Encoding]::new($false))
    & git -C $sourceGuardRoot add -- source.txt
    & git -C $sourceGuardRoot commit --quiet -m 'fixture source'
    if ($LASTEXITCODE -ne 0) { throw 'Could not create source-guard test repository.' }
    $guardCommit = (& git -C $sourceGuardRoot rev-parse HEAD).Trim().ToLowerInvariant()
    $productionRepoRoot = $repoRoot
    try {
        $repoRoot = $sourceGuardRoot
        if ((Assert-ExpectedCleanSource -ExpectedCommit $guardCommit -Phase 'clean fixture') -cne $guardCommit) {
            throw 'Source guard did not return the exact clean fixture commit.'
        }

        (Get-Item -LiteralPath $guardFile).LastWriteTimeUtc = [DateTime]::UtcNow
        Assert-ExpectedCleanSource -ExpectedCommit $guardCommit -Phase 'timestamp-only fixture' | Out-Null

        $untrackedGuard = Join-Path $sourceGuardRoot 'untracked.txt'
        [IO.File]::WriteAllText($untrackedGuard, 'untracked', [Text.UTF8Encoding]::new($false))
        Assert-ThrowsLike -Pattern '*source is not clean*untracked*' -Action {
            Assert-ExpectedCleanSource -ExpectedCommit $guardCommit -Phase 'untracked fixture' | Out-Null
        }
        Remove-Item -LiteralPath $untrackedGuard -Force

        [IO.File]::WriteAllText($guardFile, 'working change', [Text.UTF8Encoding]::new($false))
        Assert-ThrowsLike -Pattern '*source is not clean*working tree*' -Action {
            Assert-ExpectedCleanSource -ExpectedCommit $guardCommit -Phase 'working fixture' | Out-Null
        }
        [IO.File]::WriteAllText($guardFile, 'committed source', [Text.UTF8Encoding]::new($false))

        [IO.File]::WriteAllText($guardFile, 'staged change', [Text.UTF8Encoding]::new($false))
        & git -C $sourceGuardRoot add -- source.txt
        Assert-ThrowsLike -Pattern '*source is not clean*index*' -Action {
            Assert-ExpectedCleanSource -ExpectedCommit $guardCommit -Phase 'staged fixture' | Out-Null
        }
        [IO.File]::WriteAllText($guardFile, 'committed source', [Text.UTF8Encoding]::new($false))
        & git -C $sourceGuardRoot add -- source.txt
        Assert-ExpectedCleanSource -ExpectedCommit $guardCommit -Phase 'restored fixture' | Out-Null
    }
    finally {
        $repoRoot = $productionRepoRoot
    }

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
        -ExpectedSourceCommit $sourceCommit `
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
    if ([string]$evidence.source_commit -cne $sourceCommit -or
        [string]$evidence.expected_source_commit -cne $sourceCommit -or
        [string]$evidence.source_clean_check -cne 'runtime-only') {
        throw 'Preparation evidence is not bound to the expected repository source commit.'
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
            -ExpectedSourceCommit $sourceCommit `
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
            -ExpectedSourceCommit $sourceCommit `
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
            -ExpectedSourceCommit $sourceCommit `
            -MinisignPath $fakeMinisignPath `
            -OutputDirectory (Join-Path $fixtureRoot 'custom-runtime-config') `
            -TauriConfigPath $configPath
    }

    $wrongSourceCommit = '0000000000000000000000000000000000000000'
    Assert-ThrowsLike -Pattern '*does not match expected source commit*' -Action {
        & $harnessScript `
            -ArtifactPath $artifactPath `
            -SignaturePath $signaturePath `
            -CandidateVersion '0.1.0' `
            -HarnessVersion '0.0.9' `
            -ExpectedSourceCommit $wrongSourceCommit `
            -MinisignPath $fakeMinisignPath `
            -OutputDirectory (Join-Path $fixtureRoot 'wrong-source') `
            -TauriConfigPath $configPath `
            -PreparationOnly
    }
}
finally {
    Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output 'Invalid updater signature harness tests passed.'
