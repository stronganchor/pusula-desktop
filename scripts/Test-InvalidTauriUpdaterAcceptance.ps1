[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $ArtifactPath,
    [Parameter(Mandatory = $true)][string] $SignaturePath,
    [Parameter(Mandatory = $true)][string] $CandidateVersion,
    [Parameter(Mandatory = $true)][string] $HarnessVersion,
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{40}$')]
    [string] $ExpectedSourceCommit,
    [Parameter(Mandatory = $true)][string] $MinisignPath,
    [Parameter(Mandatory = $true)][string] $OutputDirectory,
    [string] $TauriConfigPath,
    [ValidateRange(0, 65535)][int] $Port = 0,
    [ValidateRange(0, 65535)][int] $DebugPort = 0,
    [ValidateRange(20, 120)][int] $ObservationTimeoutSeconds = 45,
    [switch] $PreparationOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'Release-SemVer.ps1')

function Get-FullPath {
    param([Parameter(Mandatory = $true)][string] $Path)
    return [IO.Path]::GetFullPath($Path).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
}

function Get-AvailableLoopbackPort {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    try {
        $listener.Start()
        return ([Net.IPEndPoint]$listener.LocalEndpoint).Port
    }
    finally {
        $listener.Stop()
    }
}

function Stop-ExactProcess {
    param(
        [Diagnostics.Process] $Process,
        [Parameter(Mandatory = $true)][string] $Label
    )
    if ($null -eq $Process) { return $null }
    try {
        $processId = $Process.Id
        $Process.Refresh()
        if (-not $Process.HasExited) {
            $Process.Kill()
            if (-not $Process.WaitForExit(10000)) {
                return "$Label process $processId did not exit within 10 seconds after termination."
            }
        }
        $Process.Refresh()
        if (-not $Process.HasExited) {
            return "$Label process $processId is still running after termination."
        }
        return $null
    }
    catch {
        return "$Label process could not be confirmed stopped: $($_.Exception.Message)"
    }
}

function Wait-LoopbackPortClosed {
    param(
        [Parameter(Mandatory = $true)][ValidateRange(1, 65535)][int] $Port,
        [ValidateRange(1, 100)][int] $AttemptCount = 50,
        [ValidateRange(1, 1000)][int] $DelayMilliseconds = 100
    )

    for ($attempt = 0; $attempt -lt $AttemptCount; $attempt += 1) {
        try {
            $listenerStillOpen = @(
                [Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners() |
                    Where-Object { $_.Port -eq $Port }
            ).Count -gt 0
        }
        catch {
            return "Could not verify that loopback port $Port closed: $($_.Exception.Message)"
        }
        if (-not $listenerStillOpen) { return $null }
        Start-Sleep -Milliseconds $DelayMilliseconds
    }
    return "Loopback port $Port remained open after harness process cleanup."
}

function Invoke-GitRead {
    param([Parameter(Mandatory = $true)][string[]] $ArgumentList)

    $output = @(& git -C $repoRoot @ArgumentList 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "Git source verification failed: $($output -join ' ')"
    }
    return (($output -join "`n").Trim())
}

function Invoke-GitQuietDiff {
    param([switch] $Cached)

    $arguments = @('diff')
    if ($Cached) { $arguments += '--cached' }
    $arguments += @('--quiet', '--no-ext-diff', '--ignore-submodules=none', '--')
    & git -C $repoRoot @arguments
    return [int]$LASTEXITCODE
}

function Resolve-RequiredExecutable {
    param(
        [Parameter(Mandatory = $true)][string] $Name,
        [Parameter(Mandatory = $true)][string] $PreferredPath
    )

    if (Test-Path -LiteralPath $PreferredPath -PathType Leaf) {
        return (Resolve-Path -LiteralPath $PreferredPath -ErrorAction Stop).Path
    }
    $command = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        throw "Required executable was not found: $Name"
    }
    return $command.Source
}

function Assert-ExpectedCleanSource {
    param(
        [Parameter(Mandatory = $true)][string] $ExpectedCommit,
        [Parameter(Mandatory = $true)][string] $Phase
    )

    $actualCommit = (Invoke-GitRead -ArgumentList @('rev-parse', '--verify', 'HEAD')).ToLowerInvariant()
    if ($actualCommit -cne $ExpectedCommit.ToLowerInvariant()) {
        throw "Repository HEAD $actualCommit does not match expected source commit $ExpectedCommit during $Phase."
    }
    $workingDiff = Invoke-GitQuietDiff
    $cachedDiff = Invoke-GitQuietDiff -Cached
    if ($workingDiff -gt 1 -or $cachedDiff -gt 1) {
        throw "Git could not verify repository cleanliness during $Phase."
    }
    $untracked = Invoke-GitRead -ArgumentList @('ls-files', '--others', '--exclude-standard')
    if ($workingDiff -eq 1 -or $cachedDiff -eq 1 -or -not [string]::IsNullOrWhiteSpace($untracked)) {
        $details = @()
        if ($workingDiff -eq 1) {
            $details += "working tree: $(Invoke-GitRead -ArgumentList @('diff', '--name-only', '--'))"
        }
        if ($cachedDiff -eq 1) {
            $details += "index: $(Invoke-GitRead -ArgumentList @('diff', '--cached', '--name-only', '--'))"
        }
        if (-not [string]::IsNullOrWhiteSpace($untracked)) {
            $details += "untracked: $untracked"
        }
        throw "Repository source is not clean during $Phase.`n$($details -join "`n")"
    }
    return $actualCommit
}

function Start-ArgumentProcess {
    param(
        [Parameter(Mandatory = $true)][string] $FilePath,
        [Parameter(Mandatory = $true)][string[]] $ArgumentList,
        [string] $WorkingDirectory,
        [hashtable] $Environment = @{}
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.UseShellExecute = $false
    if ($WorkingDirectory) { $startInfo.WorkingDirectory = $WorkingDirectory }
    foreach ($argument in $ArgumentList) {
        if ($argument.Contains('"')) { throw 'Harness child-process arguments cannot contain quotation marks.' }
    }
    $startInfo.Arguments = (($ArgumentList | ForEach-Object { '"' + $_ + '"' }) -join ' ')
    foreach ($name in $Environment.Keys) { $startInfo.EnvironmentVariables[$name] = [string]$Environment[$name] }
    return [Diagnostics.Process]::Start($startInfo)
}

function Write-JsonFile {
    param(
        [Parameter(Mandatory = $true)] $Value,
        [Parameter(Mandatory = $true)][string] $Path
    )
    $json = $Value | ConvertTo-Json -Depth 12
    [IO.File]::WriteAllText($Path, $json, [Text.UTF8Encoding]::new($false))
}

function Remove-HarnessPath {
    param(
        [Parameter(Mandatory = $true)][string] $Path,
        [switch] $Recurse
    )

    for ($attempt = 0; $attempt -lt 5; $attempt += 1) {
        if (-not (Test-Path -LiteralPath $Path)) { return $null }
        try {
            if ($Recurse) {
                Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Stop
            }
            else {
                Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
            }
        }
        catch {
            if ($attempt -eq 4) { return "$Path [$($_.Exception.Message)]" }
            Start-Sleep -Milliseconds 250
        }
    }
    if (Test-Path -LiteralPath $Path) { return "$Path [path still exists]" }
    return $null
}

$parsedCandidate = ConvertTo-StrictSemVer -Version $CandidateVersion
$parsedHarness = ConvertTo-StrictSemVer -Version $HarnessVersion
if ($null -ne $parsedCandidate.Prerelease -or $null -ne $parsedHarness.Prerelease) {
    throw 'The candidate and isolated harness versions must both be final SemVer values.'
}
if ((Compare-StrictSemVer -Left $HarnessVersion -Right $CandidateVersion) -ge 0) {
    throw 'The isolated harness version must be lower than the candidate so the updater offers it.'
}
$ExpectedSourceCommit = $ExpectedSourceCommit.ToLowerInvariant()
$sourceCommit = (Invoke-GitRead -ArgumentList @('rev-parse', '--verify', 'HEAD')).ToLowerInvariant()
if ($sourceCommit -cne $ExpectedSourceCommit) {
    throw "Repository HEAD $sourceCommit does not match expected source commit $ExpectedSourceCommit."
}
& (Join-Path $PSScriptRoot 'Test-VersionConsistency.ps1') -ExpectedVersion $CandidateVersion | Out-Host

$artifact = (Resolve-Path -LiteralPath $ArtifactPath -ErrorAction Stop).Path
$signature = (Resolve-Path -LiteralPath $SignaturePath -ErrorAction Stop).Path
$minisign = (Resolve-Path -LiteralPath $MinisignPath -ErrorAction Stop).Path
$expectedArtifactName = "Pusula_${CandidateVersion}_x64-setup.exe"
if ([IO.Path]::GetFileName($artifact) -cne $expectedArtifactName) {
    throw "The updater artifact must have its exact release name: $expectedArtifactName"
}
if ([IO.Path]::GetFileName($signature) -cne "$expectedArtifactName.sig") {
    throw "The updater signature must have its exact release name: $expectedArtifactName.sig"
}
$artifactInfo = Get-Item -LiteralPath $artifact
if ($artifactInfo.Length -lt 2) { throw 'The updater artifact is too small to tamper safely.' }

if (-not $TauriConfigPath) {
    $TauriConfigPath = Join-Path $repoRoot 'src-tauri\tauri.conf.json'
}
$tauriConfig = (Resolve-Path -LiteralPath $TauriConfigPath -ErrorAction Stop).Path
$productionTauriConfig = (Resolve-Path -LiteralPath (Join-Path $repoRoot 'src-tauri\tauri.conf.json')).Path
if (-not $PreparationOnly -and $tauriConfig -cne $productionTauriConfig) {
    throw 'A runtime acceptance test must inherit the repository production Tauri public key and configuration.'
}
$baseConfig = Get-Content -Raw -LiteralPath $tauriConfig | ConvertFrom-Json
$baseUpdater = $baseConfig.plugins.updater
if ([string]::IsNullOrWhiteSpace([string]$baseUpdater.pubkey)) {
    throw 'The base Tauri updater public key is empty.'
}
foreach ($endpoint in @($baseUpdater.endpoints)) {
    $endpointUri = [Uri][string]$endpoint
    if ($endpointUri.Scheme -cne 'https') {
        throw "The base Tauri updater endpoint is not HTTPS: $endpoint"
    }
}
foreach ($dangerousName in @(
        'dangerousInsecureTransportProtocol',
        'dangerousAcceptInvalidCerts',
        'dangerousAcceptInvalidHostnames'
    )) {
    $property = $baseUpdater.PSObject.Properties[$dangerousName]
    if ($null -ne $property -and [bool]$property.Value) {
        throw "The base Tauri updater configuration enables $dangerousName."
    }
}
try {
    $publicKeyBytes = [Convert]::FromBase64String([string]$baseUpdater.pubkey)
}
catch {
    throw 'The base Tauri updater public key is not valid base64.'
}
$publicKeyText = [Text.Encoding]::UTF8.GetString($publicKeyBytes)
if (-not $publicKeyText.StartsWith('untrusted comment:', [StringComparison]::Ordinal)) {
    throw 'The base Tauri updater public key is not a Minisign document.'
}

$output = Get-FullPath -Path $OutputDirectory
$repo = Get-FullPath -Path $repoRoot
if ($output -ceq $repo -or $output.StartsWith("$repo$([IO.Path]::DirectorySeparatorChar)", [StringComparison]::OrdinalIgnoreCase)) {
    throw 'The invalid-signature harness output must be outside the repository.'
}
if (Test-Path -LiteralPath $output) {
    throw "The invalid-signature harness requires a new output directory: $output"
}

if ($Port -eq 0) { $Port = Get-AvailableLoopbackPort }
if ($DebugPort -eq 0) {
    do { $DebugPort = Get-AvailableLoopbackPort } while ($DebugPort -eq $Port)
}
if ($Port -lt 1024 -or $DebugPort -lt 1024) {
    throw 'The HTTP and WebView debug ports must be unprivileged ports (1024 or greater).'
}
if ($Port -eq $DebugPort) { throw 'The HTTP and WebView debug ports must differ.' }

$serverProcess = $null
$appProcess = $null
$serverDirectory = Join-Path $output 'loopback-fixture'
$tamperedArtifact = Join-Path $serverDirectory $expectedArtifactName
$overridePath = Join-Path $output 'isolated-tauri-config.json'
$requestLogPath = Join-Path $output 'loopback-requests.jsonl'
$evidencePath = Join-Path $output 'invalid-signature-evidence.json'
$cargoTarget = Join-Path $output 'cargo-target'
$identifier = "com.stronganchor.pusula.invalid-signature-test.$([Guid]::NewGuid().ToString('N'))"
$isolatedDataPath = Join-Path $env:LOCALAPPDATA $identifier
$runtimeHelper = Join-Path $PSScriptRoot 'Invoke-InvalidUpdaterRuntime.mjs'
$candidateHashBefore = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifact).Hash.ToLowerInvariant()
$signatureHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $signature).Hash.ToLowerInvariant()
$tauriConfigHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $tauriConfig).Hash.ToLowerInvariant()
$completed = $false
$preparationOnlyCompleted = $false

try {
    [IO.Directory]::CreateDirectory($serverDirectory) | Out-Null
    [IO.File]::WriteAllText(
        (Join-Path $output 'NON-DISTRIBUTABLE.txt'),
        "LOCAL INVALID-SIGNATURE ACCEPTANCE FIXTURE`r`nNever install, publish, upload, or distribute files from this directory.`r`n",
        [Text.UTF8Encoding]::new($false)
    )

    & (Join-Path $PSScriptRoot 'Test-TauriUpdaterSignature.ps1') `
        -ArtifactPath $artifact `
        -SignaturePath $signature `
        -TauriConfigPath $tauriConfig `
        -MinisignPath $minisign | Out-Host

    Copy-Item -LiteralPath $artifact -Destination $tamperedArtifact
    $stream = [IO.File]::Open($tamperedArtifact, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try {
        $offset = [long][Math]::Floor($stream.Length / 2)
        $stream.Position = $offset
        $originalByte = $stream.ReadByte()
        if ($originalByte -lt 0) { throw 'Could not read the selected updater artifact byte.' }
        $stream.Position = $offset
        $stream.WriteByte([byte]($originalByte -bxor 1))
        $stream.Flush($true)
    }
    finally {
        $stream.Dispose()
    }
    $tamperedHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $tamperedArtifact).Hash.ToLowerInvariant()
    if ($tamperedHash -ceq $candidateHashBefore) { throw 'The isolated updater payload was not changed.' }

    $staticRejection = $null
    try {
        & (Join-Path $PSScriptRoot 'Test-TauriUpdaterSignature.ps1') `
            -ArtifactPath $tamperedArtifact `
            -SignaturePath $signature `
            -TauriConfigPath $tauriConfig `
            -MinisignPath $minisign
    }
    catch {
        $staticRejection = $_.Exception.Message
    }
    if ([string]::IsNullOrWhiteSpace($staticRejection) -or
        $staticRejection -notlike 'Tauri updater signature verification failed with exit code *') {
        throw 'The deliberately changed local copy was not rejected by the detached-signature verifier.'
    }

    $signatureText = (Get-Content -Raw -LiteralPath $signature).Trim()
    $manifestUrl = "http://127.0.0.1:$Port/latest.json"
    $artifactUrl = "http://127.0.0.1:$Port/$expectedArtifactName"
    $manifest = [ordered]@{
        version = $CandidateVersion
        notes = 'NON-DISTRIBUTABLE invalid-signature acceptance fixture'
        pub_date = [DateTimeOffset]::UtcNow.ToString('o')
        platforms = [ordered]@{
            'windows-x86_64' = [ordered]@{
                signature = $signatureText
                url = $artifactUrl
            }
        }
    }
    Write-JsonFile -Value $manifest -Path (Join-Path $serverDirectory 'latest.json')

    $override = [ordered]@{
        productName = 'Pusula Invalid Signature Acceptance'
        version = $HarnessVersion
        identifier = $identifier
        app = [ordered]@{
            windows = @([ordered]@{
                    title = 'Pusula Invalid Signature Acceptance - NON-DISTRIBUTABLE'
                    width = 1200
                    height = 800
                })
        }
        bundle = [ordered]@{
            active = $false
            createUpdaterArtifacts = $false
        }
        plugins = [ordered]@{
            updater = [ordered]@{
                endpoints = @($manifestUrl)
            }
        }
    }
    Write-JsonFile -Value $override -Path $overridePath

    $candidateHashAfterPreparation = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifact).Hash.ToLowerInvariant()
    if ($candidateHashAfterPreparation -cne $candidateHashBefore) {
        throw 'The source candidate artifact changed while preparing the isolated copy.'
    }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $signature).Hash.ToLowerInvariant() -cne $signatureHash) {
        throw 'The source candidate signature changed while preparing the isolated copy.'
    }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $tauriConfig).Hash.ToLowerInvariant() -cne $tauriConfigHash) {
        throw 'The Tauri configuration changed while preparing the isolated copy.'
    }

    if ($PreparationOnly) {
        $preparationEvidence = [ordered]@{
            schema_version = 1
            result = 'preparation-only'
            runtime_executed = $false
            source_commit = $sourceCommit
            expected_source_commit = $ExpectedSourceCommit
            source_clean_check = 'runtime-only'
            candidate_version = $CandidateVersion
            harness_version = $HarnessVersion
            candidate_artifact = $expectedArtifactName
            candidate_sha256 = $candidateHashBefore
            tampered_copy_sha256 = $tamperedHash
            signature_sha256 = $signatureHash
            candidate_unchanged = $true
            signature_unchanged = $true
            original_signature_verification = 'accepted'
            tampered_copy_signature_verification = 'rejected'
            isolated_identifier = $identifier
            loopback_manifest = $manifestUrl
            dangerous_updater_overrides = $false
            production_configuration_modified = $false
            installer_created_or_run = $false
        }
        Write-JsonFile -Value $preparationEvidence -Path $evidencePath
        Remove-Item -LiteralPath $tamperedArtifact -Force
        $preparationOnlyCompleted = $true
        Write-Output "Invalid-signature harness preparation checks passed: $evidencePath"
        return
    }

    $node = (Get-Command node -ErrorAction Stop).Source
    $npm = (Get-Command npm.cmd -ErrorAction Stop).Source
    $rustupBin = Join-Path $env:USERPROFILE '.cargo\bin'
    $cargo = Resolve-RequiredExecutable `
        -Name 'cargo.exe' `
        -PreferredPath (Join-Path $rustupBin 'cargo.exe')
    $rustc = Resolve-RequiredExecutable `
        -Name 'rustc.exe' `
        -PreferredPath (Join-Path $rustupBin 'rustc.exe')
    $sourceCommit = Assert-ExpectedCleanSource `
        -ExpectedCommit $ExpectedSourceCommit `
        -Phase 'before the isolated runtime build'
    $oldCargoTarget = $env:CARGO_TARGET_DIR
    $oldCargoCommand = $env:CARGO
    $oldPath = $env:PATH
    try {
        $env:CARGO_TARGET_DIR = $cargoTarget
        $env:CARGO = $cargo
        $toolDirectories = @((Split-Path $cargo -Parent), (Split-Path $rustc -Parent)) |
            Select-Object -Unique
        $env:PATH = ((@($toolDirectories) + @($oldPath)) -join [IO.Path]::PathSeparator)
        & $cargo --version | Out-Host
        if ($LASTEXITCODE -ne 0) { throw 'The isolated build Rust toolchain is not executable.' }
        Push-Location $repoRoot
        try {
            & $npm run tauri -- build --debug --no-bundle --ci --config $overridePath
            if ($LASTEXITCODE -ne 0) { throw 'The isolated non-distributable debug build failed.' }
        }
        finally {
            Pop-Location
        }
    }
    finally {
        $env:CARGO_TARGET_DIR = $oldCargoTarget
        $env:CARGO = $oldCargoCommand
        $env:PATH = $oldPath
    }

    $debugExecutable = Join-Path $cargoTarget 'debug\pusula-desktop.exe'
    if (-not (Test-Path -LiteralPath $debugExecutable -PathType Leaf)) {
        throw "The isolated debug executable was not created: $debugExecutable"
    }

    $serverProcess = Start-ArgumentProcess `
        -FilePath $node `
        -ArgumentList @($runtimeHelper, 'serve', $serverDirectory, $expectedArtifactName, [string]$Port, $requestLogPath) `
        -WorkingDirectory $repoRoot

    $serverReady = $false
    for ($attempt = 0; $attempt -lt 50; $attempt += 1) {
        if ($serverProcess.HasExited) { throw "The loopback updater server exited with code $($serverProcess.ExitCode)." }
        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri $manifestUrl -TimeoutSec 2
            if ($response.StatusCode -eq 200) {
                $serverReady = $true
                break
            }
        }
        catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $serverReady) { throw 'The loopback updater server did not become ready.' }

    $appProcess = Start-ArgumentProcess `
        -FilePath $debugExecutable `
        -ArgumentList @() `
        -WorkingDirectory $repoRoot `
        -Environment @{
            WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort"
        }

    $runtimeJson = & $node $runtimeHelper observe ([string]$DebugPort) ([string]$ObservationTimeoutSeconds)
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace(($runtimeJson -join "`n"))) {
        throw 'The isolated WebView observation did not complete.'
    }
    $runtime = ($runtimeJson -join "`n") | ConvertFrom-Json
    if ([bool]$runtime.confirmation_called) {
        throw 'The invalid payload reached installation confirmation instead of being rejected during download verification.'
    }
    if ([string]$runtime.rejection_warning -notlike 'Pusula update failed during downloading*') {
        throw 'The application did not report rejection during the updater download/verification phase.'
    }
    if ([string]$runtime.rejection_warning -notlike '*The signature verification failed*') {
        throw 'The runtime failure was not the expected Tauri invalid-signature rejection.'
    }

    Start-Sleep -Milliseconds 250
    $requests = @(Get-Content -LiteralPath $requestLogPath | ForEach-Object { $_ | ConvertFrom-Json })
    $manifestRequest = @($requests | Where-Object { $_.method -ceq 'GET' -and $_.path -ceq '/latest.json' -and $_.status -eq 200 })
    $artifactRequest = @($requests | Where-Object { $_.method -ceq 'GET' -and $_.path -ceq "/$expectedArtifactName" -and $_.status -eq 200 })
    if ($manifestRequest.Count -lt 1 -or $artifactRequest.Count -lt 1) {
        throw 'The isolated app did not fetch both the loopback manifest and deliberately invalid payload.'
    }

    $candidateHashAfterRuntime = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifact).Hash.ToLowerInvariant()
    if ($candidateHashAfterRuntime -cne $candidateHashBefore) {
        throw 'The source candidate artifact changed during the runtime rejection test.'
    }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $signature).Hash.ToLowerInvariant() -cne $signatureHash) {
        throw 'The source candidate signature changed during the runtime rejection test.'
    }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $tauriConfig).Hash.ToLowerInvariant() -cne $tauriConfigHash) {
        throw 'The production Tauri configuration changed during the runtime rejection test.'
    }
    $sourceCommitAfterRuntime = Assert-ExpectedCleanSource `
        -ExpectedCommit $ExpectedSourceCommit `
        -Phase 'after the isolated runtime test'
    if ($sourceCommitAfterRuntime -cne $sourceCommit) {
        throw 'The repository source commit changed during the runtime rejection test.'
    }

    $evidence = [ordered]@{
        schema_version = 1
        test = 'tauri-invalid-signature-rejection'
        result = 'pass'
        completed_at_utc = [DateTimeOffset]::UtcNow.ToString('o')
        source_commit = $sourceCommit
        expected_source_commit = $ExpectedSourceCommit
        source_clean = $true
        candidate_version = $CandidateVersion
        harness_version = $HarnessVersion
        candidate_artifact = $expectedArtifactName
        candidate_sha256 = $candidateHashBefore
        tampered_copy_sha256 = $tamperedHash
        signature_sha256 = $signatureHash
        candidate_unchanged = $true
        signature_unchanged = $true
        original_signature_verification = 'accepted'
        tampered_copy_signature_verification = 'rejected'
        runtime_rejection_phase = 'downloading'
        runtime_warning = [string]$runtime.rejection_warning
        installation_confirmation_called = [bool]$runtime.confirmation_called
        loopback_manifest_requests = $manifestRequest.Count
        loopback_artifact_requests = $artifactRequest.Count
        isolated_identifier = $identifier
        runtime_build = 'debug-no-bundle'
        transport = 'http-loopback-debug-only'
        dangerous_updater_overrides = $false
        production_configuration_modified = $false
        installer_created_or_run = $false
    }
    $completed = $true
}
finally {
    $cleanupProblems = @()

    if ($null -ne $appProcess) {
        $cleanupProblem = Stop-ExactProcess -Process $appProcess -Label 'isolated app'
        if ($cleanupProblem) { $cleanupProblems += $cleanupProblem }
        $cleanupProblem = Wait-LoopbackPortClosed -Port $DebugPort
        if ($cleanupProblem) { $cleanupProblems += $cleanupProblem }
    }
    if ($null -ne $serverProcess) {
        $cleanupProblem = Stop-ExactProcess -Process $serverProcess -Label 'loopback updater server'
        if ($cleanupProblem) { $cleanupProblems += $cleanupProblem }
        $cleanupProblem = Wait-LoopbackPortClosed -Port $Port
        if ($cleanupProblem) { $cleanupProblems += $cleanupProblem }
    }

    if ($identifier.StartsWith('com.stronganchor.pusula.invalid-signature-test.', [StringComparison]::Ordinal) -and
        (Test-Path -LiteralPath $isolatedDataPath)) {
        $localRoot = Get-FullPath -Path $env:LOCALAPPDATA
        $isolatedFull = Get-FullPath -Path $isolatedDataPath
        if ($isolatedFull.StartsWith("$localRoot$([IO.Path]::DirectorySeparatorChar)com.stronganchor.pusula.invalid-signature-test.", [StringComparison]::OrdinalIgnoreCase)) {
            $cleanupProblem = Remove-HarnessPath -Path $isolatedFull -Recurse
            if ($cleanupProblem) { $cleanupProblems += $cleanupProblem }
        }
    }

    if (-not $PreparationOnly -or -not $preparationOnlyCompleted) {
        foreach ($directory in @($serverDirectory, $cargoTarget)) {
            $cleanupProblem = Remove-HarnessPath -Path $directory -Recurse
            if ($cleanupProblem) { $cleanupProblems += $cleanupProblem }
        }
        foreach ($file in @($overridePath, $requestLogPath, (Join-Path $output 'NON-DISTRIBUTABLE.txt'))) {
            $cleanupProblem = Remove-HarnessPath -Path $file
            if ($cleanupProblem) { $cleanupProblems += $cleanupProblem }
        }
    }
    if (-not $completed -and -not $preparationOnlyCompleted -and (Test-Path -LiteralPath $output)) {
        $cleanupProblem = Remove-HarnessPath -Path $output -Recurse
        if ($cleanupProblem) { $cleanupProblems += $cleanupProblem }
    }
    if ($cleanupProblems.Count -gt 0) {
        if ($completed -and (Test-Path -LiteralPath $output)) {
            $cleanupFailurePath = Join-Path $output 'invalid-signature-cleanup-failure.txt'
            [IO.File]::WriteAllText(
                $cleanupFailurePath,
                "INVALID-SIGNATURE HARNESS DID NOT PASS CLEANUP`r`n$($cleanupProblems -join "`r`n")`r`n",
                [Text.UTF8Encoding]::new($false)
            )
        }
        $completed = $false
        throw "Invalid-signature harness cleanup failed: $($cleanupProblems -join '; ')"
    }
}

if ($completed) {
    Write-JsonFile -Value $evidence -Path $evidencePath
    $evidenceHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $evidencePath).Hash.ToLowerInvariant()
    Write-Output 'Invalid Tauri updater signature rejected before confirmation.'
    Write-Output "Evidence: $evidencePath"
    Write-Output "Evidence SHA-256: $evidenceHash"
}
