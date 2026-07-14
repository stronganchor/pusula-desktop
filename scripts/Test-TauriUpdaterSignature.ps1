[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $ArtifactPath,
    [Parameter(Mandatory = $true)][string] $SignaturePath,
    [Parameter(Mandatory = $true)][string] $TauriConfigPath,
    [Parameter(Mandatory = $true)][string] $MinisignPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$artifact = (Resolve-Path -LiteralPath $ArtifactPath -ErrorAction Stop).Path
$signatureFile = (Resolve-Path -LiteralPath $SignaturePath -ErrorAction Stop).Path
$tauriConfig = (Resolve-Path -LiteralPath $TauriConfigPath -ErrorAction Stop).Path
$minisign = (Resolve-Path -LiteralPath $MinisignPath -ErrorAction Stop).Path

$config = Get-Content -Raw -LiteralPath $tauriConfig | ConvertFrom-Json
$encodedPublicKey = [string]$config.plugins.updater.pubkey
$encodedSignature = (Get-Content -Raw -LiteralPath $signatureFile).Trim()
if ([string]::IsNullOrWhiteSpace($encodedPublicKey) -or [string]::IsNullOrWhiteSpace($encodedSignature)) {
    throw 'Updater public key or signature is empty.'
}
if ($encodedPublicKey.Length -gt 65536 -or $encodedSignature.Length -gt 65536) {
    throw 'Updater public key or signature is unexpectedly large.'
}

$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ('pusula-updater-verify-' + [Guid]::NewGuid().ToString('N'))
try {
    [IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
    $publicKeyPath = Join-Path $temporaryDirectory 'updater.pub'
    $decodedSignaturePath = Join-Path $temporaryDirectory 'updater.minisig'
    try {
        [IO.File]::WriteAllBytes($publicKeyPath, [Convert]::FromBase64String($encodedPublicKey))
        [IO.File]::WriteAllBytes($decodedSignaturePath, [Convert]::FromBase64String($encodedSignature))
    }
    catch {
        throw 'Updater public key or signature is not valid base64.'
    }

    $publicKeyText = Get-Content -Raw -LiteralPath $publicKeyPath
    $signatureText = Get-Content -Raw -LiteralPath $decodedSignaturePath
    if (-not $publicKeyText.StartsWith('untrusted comment:', [StringComparison]::Ordinal) -or
        -not $signatureText.StartsWith('untrusted comment:', [StringComparison]::Ordinal)) {
        throw 'Decoded updater key or signature is not a Minisign document.'
    }

    & $minisign -V -m $artifact -p $publicKeyPath -x $decodedSignaturePath
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri updater signature verification failed with exit code $LASTEXITCODE."
    }
}
finally {
    Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output "Tauri updater signature verified: $artifact"
