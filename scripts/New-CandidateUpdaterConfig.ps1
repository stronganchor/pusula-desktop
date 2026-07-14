[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][Alias('Version')][string] $CandidateVersion,
    [string] $ApplicationVersion,
    [string] $Repository = 'stronganchor/pusula-desktop',
    [Parameter(Mandatory = $true)][string] $OutputPath,
    [switch] $Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Release-SemVer.ps1')
$candidate = ConvertTo-StrictSemVer -Version $CandidateVersion
if ($null -ne $candidate.Prerelease) {
    throw 'The candidate updater target must use a final SemVer; GitHub prerelease is the publication state.'
}
if (-not [string]::IsNullOrWhiteSpace($ApplicationVersion)) {
    $application = ConvertTo-StrictSemVer -Version $ApplicationVersion
    if ($null -ne $application.Prerelease) {
        throw 'The acceptance baseline application version must be a final SemVer.'
    }
    if ((Compare-StrictSemVer -Left $ApplicationVersion -Right $CandidateVersion) -ge 0) {
        throw "Acceptance baseline version $ApplicationVersion must be lower than candidate $CandidateVersion."
    }
}
if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw "Invalid GitHub repository name: $Repository"
}
if ((Test-Path -LiteralPath $OutputPath) -and -not $Force) {
    throw "Candidate updater config already exists: $OutputPath"
}

$endpoint = "https://github.com/$Repository/releases/download/v$CandidateVersion/latest.json"
$config = [ordered]@{
    plugins = [ordered]@{
        updater = [ordered]@{
            endpoints = @($endpoint)
        }
    }
}
if (-not [string]::IsNullOrWhiteSpace($ApplicationVersion)) {
    $config.Insert(0, 'version', $ApplicationVersion)
}
$parent = [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($OutputPath))
[IO.Directory]::CreateDirectory($parent) | Out-Null
$config | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $OutputPath -Encoding utf8
$mode = if ([string]::IsNullOrWhiteSpace($ApplicationVersion)) {
    'candidate updater override'
}
else {
    "acceptance baseline $ApplicationVersion"
}
Write-Output "$mode created for $endpoint"
