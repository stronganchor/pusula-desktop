[CmdletBinding()]
param(
    [Parameter(Mandatory, Position = 0)]
    [string] $Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-RequiredEnvironmentValue {
    param([Parameter(Mandatory)][string] $Name)

    $value = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrWhiteSpace($value)) {
        throw "Required environment variable is missing: $Name"
    }

    return $value
}

$endpoint = Get-RequiredEnvironmentValue -Name 'ARTIFACT_SIGNING_ENDPOINT'
$account = Get-RequiredEnvironmentValue -Name 'ARTIFACT_SIGNING_ACCOUNT'
$profile = Get-RequiredEnvironmentValue -Name 'ARTIFACT_SIGNING_PROFILE'
$dlibPath = Get-RequiredEnvironmentValue -Name 'ARTIFACT_SIGNING_DLIB_PATH'
$signToolPath = Get-RequiredEnvironmentValue -Name 'SIGNTOOL_PATH'
$runnerTemp = Get-RequiredEnvironmentValue -Name 'RUNNER_TEMP'

$endpointUri = [Uri]$endpoint
if ($endpointUri.Scheme -ne 'https' -or -not $endpointUri.Host.EndsWith('.codesigning.azure.net', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'ARTIFACT_SIGNING_ENDPOINT must be an HTTPS Azure Artifact Signing endpoint.'
}

foreach ($toolPath in $dlibPath, $signToolPath) {
    if (-not (Test-Path -LiteralPath $toolPath -PathType Leaf)) {
        throw "Signing dependency was not found: $toolPath"
    }
}

$target = Resolve-Path -LiteralPath $Path -ErrorAction Stop
$supportedExtensions = '.exe', '.dll', '.msi', '.msix', '.appx', '.cab'
if ([IO.Path]::GetExtension($target.Path).ToLowerInvariant() -notin $supportedExtensions) {
    throw "Refusing to sign unsupported file type: $($target.Path)"
}

$metadataPath = Join-Path $runnerTemp "pusula-artifact-signing-$PID-$([Guid]::NewGuid().ToString('N')).json"
$metadata = [ordered]@{
    Endpoint = $endpointUri.AbsoluteUri.TrimEnd('/')
    CodeSigningAccountName = $account
    CertificateProfileName = $profile
    ExcludeCredentials = @(
        'EnvironmentCredential'
        'ManagedIdentityCredential'
        'WorkloadIdentityCredential'
        'SharedTokenCacheCredential'
        'VisualStudioCredential'
        'VisualStudioCodeCredential'
        'AzurePowerShellCredential'
        'AzureDeveloperCliCredential'
        'InteractiveBrowserCredential'
    )
}

try {
    $metadata | ConvertTo-Json -Depth 4 -Compress | Set-Content -LiteralPath $metadataPath -Encoding utf8NoBOM

    & $signToolPath sign /v /fd SHA256 /tr 'http://timestamp.acs.microsoft.com' /td SHA256 /dlib $dlibPath /dmdf $metadataPath $target.Path
    if ($LASTEXITCODE -ne 0) {
        throw "Artifact Signing failed for $($target.Path) with exit code $LASTEXITCODE."
    }

    & $signToolPath verify /pa /all /v $target.Path
    if ($LASTEXITCODE -ne 0) {
        throw "Authenticode verification failed for $($target.Path) with exit code $LASTEXITCODE."
    }
} finally {
    Remove-Item -LiteralPath $metadataPath -Force -ErrorAction SilentlyContinue
}
