Set-StrictMode -Version Latest

$script:StrictSemVerPattern = [regex]::new(
    '^(?<major>0|[1-9][0-9]*)\.(?<minor>0|[1-9][0-9]*)\.(?<patch>0|[1-9][0-9]*)(?:-(?<prerelease>(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?$'
)

function ConvertTo-StrictSemVer {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string] $Version)

    $match = $script:StrictSemVerPattern.Match($Version)
    if (-not $match.Success) {
        throw "Release version must be strict SemVer without a v prefix or build metadata: $Version"
    }

    $parts = [ordered]@{}
    foreach ($name in 'major', 'minor', 'patch') {
        $parsed = 0
        if (-not [int]::TryParse($match.Groups[$name].Value, [ref] $parsed)) {
            throw "Release version component is outside the supported 32-bit range: $Version"
        }
        $parts[$name] = $parsed
    }

    [pscustomobject][ordered]@{
        Original = $Version
        Major = $parts.major
        Minor = $parts.minor
        Patch = $parts.patch
        Prerelease = if ($match.Groups['prerelease'].Success) {
            $match.Groups['prerelease'].Value
        }
        else {
            $null
        }
    }
}

function Compare-StrictSemVer {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string] $Left,
        [Parameter(Mandatory = $true)][string] $Right
    )

    $leftVersion = ConvertTo-StrictSemVer -Version $Left
    $rightVersion = ConvertTo-StrictSemVer -Version $Right

    foreach ($name in 'Major', 'Minor', 'Patch') {
        if ($leftVersion.$name -lt $rightVersion.$name) { return -1 }
        if ($leftVersion.$name -gt $rightVersion.$name) { return 1 }
    }

    if ($null -eq $leftVersion.Prerelease -and $null -eq $rightVersion.Prerelease) { return 0 }
    if ($null -eq $leftVersion.Prerelease) { return 1 }
    if ($null -eq $rightVersion.Prerelease) { return -1 }

    $leftIdentifiers = @($leftVersion.Prerelease -split '\.')
    $rightIdentifiers = @($rightVersion.Prerelease -split '\.')
    $length = [Math]::Max($leftIdentifiers.Count, $rightIdentifiers.Count)
    for ($index = 0; $index -lt $length; $index += 1) {
        if ($index -ge $leftIdentifiers.Count) { return -1 }
        if ($index -ge $rightIdentifiers.Count) { return 1 }

        $leftIdentifier = $leftIdentifiers[$index]
        $rightIdentifier = $rightIdentifiers[$index]
        $leftNumeric = $leftIdentifier -match '^[0-9]+$'
        $rightNumeric = $rightIdentifier -match '^[0-9]+$'

        if ($leftNumeric -and $rightNumeric) {
            if ($leftIdentifier.Length -lt $rightIdentifier.Length) { return -1 }
            if ($leftIdentifier.Length -gt $rightIdentifier.Length) { return 1 }
            $comparison = [string]::CompareOrdinal($leftIdentifier, $rightIdentifier)
        }
        elseif ($leftNumeric) {
            return -1
        }
        elseif ($rightNumeric) {
            return 1
        }
        else {
            $comparison = [string]::CompareOrdinal($leftIdentifier, $rightIdentifier)
        }

        if ($comparison -lt 0) { return -1 }
        if ($comparison -gt 0) { return 1 }
    }

    return 0
}

function Get-ReleaseCandidateTag {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string] $Version,
        [Parameter(Mandatory = $true)][string] $Commit
    )

    $parsed = ConvertTo-StrictSemVer -Version $Version
    if ($null -ne $parsed.Prerelease) {
        throw 'A release candidate tag must be based on the final SemVer without a prerelease suffix.'
    }
    if ($Commit -cnotmatch '^[0-9a-f]{40}$') {
        throw 'A release candidate tag requires a lowercase full 40-character Git SHA.'
    }

    return "v$Version-candidate.$Commit"
}
