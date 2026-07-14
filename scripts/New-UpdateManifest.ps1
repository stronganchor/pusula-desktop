param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$SignaturePath,
    [Parameter(Mandatory = $true)][string]$ArtifactName,
    [Parameter(Mandatory = $true)][string]$Repository,
    [Parameter(Mandatory = $true)][string]$OutputPath
)

$ErrorActionPreference = 'Stop'

if ($Version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$') {
    throw "Invalid semantic version: $Version"
}

$signature = (Get-Content -Raw -LiteralPath $SignaturePath).Trim()
if ([string]::IsNullOrWhiteSpace($signature)) {
    throw 'Updater signature is empty.'
}

$downloadUrl = "https://github.com/$Repository/releases/download/v$Version/$ArtifactName"
$manifest = [ordered]@{
    version = $Version
    notes = "Pusula $Version"
    pub_date = [DateTimeOffset]::UtcNow.ToString('o')
    platforms = [ordered]@{
        'windows-x86_64' = [ordered]@{
            signature = $signature
            url = $downloadUrl
        }
    }
}

$manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $OutputPath -Encoding UTF8

