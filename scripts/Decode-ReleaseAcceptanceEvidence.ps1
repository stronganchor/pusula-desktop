[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Base64,
    [Parameter(Mandatory = $true)][string] $ExpectedSha256,
    [Parameter(Mandatory = $true)][string] $OutputPath,
    [switch] $Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-AcceptanceEvidence.ps1')

if ($ExpectedSha256 -cnotmatch '^[0-9a-fA-F]{64}$') {
    throw 'Expected acceptance evidence SHA-256 must be 64 hexadecimal characters.'
}
if ((Test-Path -LiteralPath $OutputPath) -and -not $Force) {
    throw "Decoded acceptance evidence already exists: $OutputPath"
}

$decoded = ConvertFrom-PusulaAcceptanceEvidenceBase64 -Base64 $Base64
$sha256 = [Security.Cryptography.SHA256]::Create()
try { $sha = $sha256.ComputeHash([byte[]]$decoded.bytes) }
finally { $sha256.Dispose() }
$actualSha256 = ([BitConverter]::ToString($sha)).Replace('-', '').ToLowerInvariant()
if ($actualSha256 -cne $ExpectedSha256.ToLowerInvariant()) {
    throw 'Decoded acceptance evidence SHA-256 does not match the supplied promotion digest.'
}

$fullPath = [IO.Path]::GetFullPath($OutputPath)
$parent = [IO.Path]::GetDirectoryName($fullPath)
if ([string]::IsNullOrWhiteSpace($parent)) { throw 'Acceptance evidence output path has no parent directory.' }
[IO.Directory]::CreateDirectory($parent) | Out-Null
[IO.File]::WriteAllBytes($fullPath, [byte[]]$decoded.bytes)

Write-Output "Decoded acceptance evidence: $fullPath"
Write-Output "SHA-256: $actualSha256"
