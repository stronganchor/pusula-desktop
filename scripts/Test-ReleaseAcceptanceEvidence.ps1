[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $EvidencePath,
    [Parameter(Mandatory = $true)][string] $ExpectedSha256,
    [Parameter(Mandatory = $true)][string] $CandidateAssetDirectory,
    [Parameter(Mandatory = $true)][string] $Repository,
    [Parameter(Mandatory = $true)][string] $Version,
    [Parameter(Mandatory = $true)][string] $CandidateTag,
    [Parameter(Mandatory = $true)][string] $CandidateCommit,
    [Parameter(Mandatory = $true)][string] $ActionsToken,
    [string] $FixturePath = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-AcceptanceEvidence.ps1')

if ([string]::IsNullOrWhiteSpace($FixturePath)) {
    $FixturePath = Join-Path (Split-Path $PSScriptRoot -Parent) 'tests\fixtures\pusula-lite-v1.json'
}
if ($ExpectedSha256 -cnotmatch '^[0-9a-fA-F]{64}$') {
    throw 'Expected acceptance evidence SHA-256 must be 64 hexadecimal characters.'
}
if ([string]::IsNullOrWhiteSpace($ActionsToken)) {
    throw 'An Actions-read token is required to bind evidence to the candidate workflow run.'
}

$strictFile = Read-PusulaStrictUtf8File -Path $EvidencePath
$actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $strictFile.path).Hash.ToLowerInvariant()
if ($actualHash -cne $ExpectedSha256.ToLowerInvariant()) {
    throw 'Acceptance evidence SHA-256 does not match the supplied promotion digest.'
}

Assert-PusulaJsonSyntaxAndUniqueProperties -Text $strictFile.text
try { $evidence = $strictFile.text | ConvertFrom-Json }
catch { throw "Acceptance evidence JSON could not be parsed: $($_.Exception.Message)" }
$canonical = Get-PusulaCanonicalAcceptanceEvidence `
    -Evidence $evidence `
    -Repository $Repository `
    -Version $Version `
    -CandidateTag $CandidateTag `
    -CandidateCommit $CandidateCommit `
    -CandidateAssetDirectory $CandidateAssetDirectory `
    -FixturePath $FixturePath
$canonicalText = ConvertTo-PusulaCanonicalJson $canonical
if (-not [string]::Equals($strictFile.text, $canonicalText, [StringComparison]::Ordinal)) {
    throw 'Acceptance evidence is valid data but is not the exact canonical compact JSON representation.'
}

$savedToken = $env:GH_TOKEN
try {
    $env:GH_TOKEN = $ActionsToken
    $runRows = @(& gh api "repos/$Repository/actions/runs/$($canonical.candidate.workflow_run_id)")
    if ($LASTEXITCODE -ne 0) { throw 'Could not read the candidate workflow run through GitHub.' }
}
finally {
    $env:GH_TOKEN = $savedToken
}
$run = ($runRows -join "`n") | ConvertFrom-Json
if ([long]$run.id -ne [long]$canonical.candidate.workflow_run_id -or
    [string]$run.event -cne 'workflow_dispatch' -or
    [string]$run.status -cne 'completed' -or
    [string]$run.conclusion -cne 'success' -or
    [string]$run.head_branch -cne 'main' -or
    [string]$run.head_sha -cne $CandidateCommit -or
    [string]$run.path -cne '.github/workflows/release.yml' -or
    [string]$run.repository.full_name -cne $Repository) {
    throw 'Acceptance evidence workflow run is not the successful candidate release run for this repository and commit.'
}

Write-Output "Canonical acceptance evidence verified: $actualHash"
