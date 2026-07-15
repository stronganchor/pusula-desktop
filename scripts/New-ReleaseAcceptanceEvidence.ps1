[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $InputPath,
    [Parameter(Mandatory = $true)][string] $OutputPath,
    [Parameter(Mandatory = $true)][string] $CandidateAssetDirectory,
    [Parameter(Mandatory = $true)][string] $Repository,
    [Parameter(Mandatory = $true)][string] $Version,
    [Parameter(Mandatory = $true)][string] $CandidateTag,
    [Parameter(Mandatory = $true)][string] $CandidateCommit,
    [Parameter(Mandatory = $true)][string] $ExpectedWindowsPublisher,
    [Parameter(Mandatory = $true)][string] $ExpectedWindowsCertificateSha256,
    [string] $FixturePath = '',
    [switch] $Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-AcceptanceEvidence.ps1')

if ([string]::IsNullOrWhiteSpace($FixturePath)) {
    $FixturePath = Join-Path (Split-Path $PSScriptRoot -Parent) 'tests\fixtures\pusula-lite-v1.json'
}
if ((Test-Path -LiteralPath $OutputPath) -and -not $Force) {
    throw "Canonical acceptance evidence already exists: $OutputPath"
}

$inputFile = Read-PusulaStrictUtf8File -Path $InputPath
Assert-PusulaJsonSyntaxAndUniqueProperties -Text $inputFile.text
try { $inputEvidence = $inputFile.text | ConvertFrom-Json }
catch { throw "Acceptance evidence input could not be parsed: $($_.Exception.Message)" }
$canonical = Get-PusulaCanonicalAcceptanceEvidence `
    -Evidence $inputEvidence `
    -Repository $Repository `
    -Version $Version `
    -CandidateTag $CandidateTag `
    -CandidateCommit $CandidateCommit `
    -CandidateAssetDirectory $CandidateAssetDirectory `
    -ExpectedWindowsPublisher $ExpectedWindowsPublisher `
    -ExpectedWindowsCertificateSha256 $ExpectedWindowsCertificateSha256 `
    -FixturePath $FixturePath
$canonicalText = ConvertTo-PusulaCanonicalJson $canonical
$outputFullPath = [IO.Path]::GetFullPath($OutputPath)
[IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($outputFullPath)) | Out-Null
[IO.File]::WriteAllText($outputFullPath, $canonicalText, [Text.UTF8Encoding]::new($false))
$outputBytes = [IO.File]::ReadAllBytes($outputFullPath)
if ($outputBytes.Length -gt $script:PusulaAcceptanceEvidenceMaximumBytes) {
    throw "Canonical acceptance evidence exceeds $script:PusulaAcceptanceEvidenceMaximumBytes decoded bytes."
}
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $outputFullPath).Hash.ToLowerInvariant()
$base64 = [Convert]::ToBase64String($outputBytes)
if ($base64.Length -gt $script:PusulaAcceptanceEvidenceMaximumBase64Characters) {
    throw "Canonical acceptance evidence exceeds $script:PusulaAcceptanceEvidenceMaximumBase64Characters base64 characters."
}

Write-Output "Canonical acceptance evidence: $outputFullPath"
Write-Output "SHA-256: $hash"
Write-Output "workflow_dispatch base64: $base64"
