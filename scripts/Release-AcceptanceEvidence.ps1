Set-StrictMode -Version Latest

$script:PusulaAcceptanceEvidenceMaximumBytes = 46080
$script:PusulaAcceptanceEvidenceMaximumBase64Characters = 61440
$script:PusulaAcceptanceFixtureManifestSha256 = 'd709a52df5147bddd57d569d1de4113f76ac10f8841405d970e4e60bdd90ade6'
$script:PusulaAcceptanceCheckNames = @(
    'offline_install',
    'fixture_import',
    'single_instance',
    'offline_business_workflow',
    'restart_persistence',
    'failure_atomicity',
    'positive_update',
    'pre_update_backup',
    'direct_backup_upload',
    'relay_backup_upload',
    'restore',
    'invalid_signature_rejection'
)

function Skip-PusulaJsonWhitespace {
    param([string] $Text, [ref] $Index)
    while ($Index.Value -lt $Text.Length -and
        " `t`r`n".IndexOf($Text[$Index.Value]) -ge 0) {
        $Index.Value += 1
    }
}

function Read-PusulaJsonString {
    param([string] $Text, [ref] $Index)

    if ($Index.Value -ge $Text.Length -or $Text[$Index.Value] -ne '"') {
        throw "Invalid JSON string at offset $($Index.Value)."
    }
    $Index.Value += 1
    $builder = New-Object Text.StringBuilder
    while ($Index.Value -lt $Text.Length) {
        $character = $Text[$Index.Value]
        $Index.Value += 1
        if ($character -eq '"') { return $builder.ToString() }
        if ([int][char]$character -lt 0x20) { throw 'JSON strings cannot contain unescaped control characters.' }
        if ($character -ne '\') {
            $null = $builder.Append($character)
            continue
        }
        if ($Index.Value -ge $Text.Length) { throw 'JSON string ends after an escape prefix.' }
        $escape = $Text[$Index.Value]
        $Index.Value += 1
        switch ($escape) {
            '"' { $null = $builder.Append('"') }
            '\' { $null = $builder.Append('\') }
            '/' { $null = $builder.Append('/') }
            'b' { $null = $builder.Append([char]8) }
            'f' { $null = $builder.Append([char]12) }
            'n' { $null = $builder.Append([char]10) }
            'r' { $null = $builder.Append([char]13) }
            't' { $null = $builder.Append([char]9) }
            'u' {
                if ($Index.Value + 4 -gt $Text.Length) { throw 'JSON Unicode escape is incomplete.' }
                $hex = $Text.Substring($Index.Value, 4)
                if ($hex -cnotmatch '^[0-9A-Fa-f]{4}$') { throw 'JSON Unicode escape is invalid.' }
                $null = $builder.Append([char][Convert]::ToUInt16($hex, 16))
                $Index.Value += 4
            }
            default { throw "Invalid JSON escape: \$escape" }
        }
    }
    throw 'JSON string is unterminated.'
}

function Read-PusulaJsonValue {
    param([string] $Text, [ref] $Index)

    Skip-PusulaJsonWhitespace -Text $Text -Index $Index
    if ($Index.Value -ge $Text.Length) { throw 'JSON value is missing.' }
    $character = $Text[$Index.Value]
    if ($character -eq '"') {
        $null = Read-PusulaJsonString -Text $Text -Index $Index
        return
    }
    if ($character -eq '{') {
        $Index.Value += 1
        $names = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
        Skip-PusulaJsonWhitespace -Text $Text -Index $Index
        if ($Index.Value -lt $Text.Length -and $Text[$Index.Value] -eq '}') {
            $Index.Value += 1
            return
        }
        while ($true) {
            Skip-PusulaJsonWhitespace -Text $Text -Index $Index
            $name = Read-PusulaJsonString -Text $Text -Index $Index
            if (-not $names.Add($name)) { throw "Duplicate JSON property is forbidden: $name" }
            Skip-PusulaJsonWhitespace -Text $Text -Index $Index
            if ($Index.Value -ge $Text.Length -or $Text[$Index.Value] -ne ':') {
                throw "JSON property $name has no colon."
            }
            $Index.Value += 1
            Read-PusulaJsonValue -Text $Text -Index $Index
            Skip-PusulaJsonWhitespace -Text $Text -Index $Index
            if ($Index.Value -ge $Text.Length) { throw 'JSON object is unterminated.' }
            if ($Text[$Index.Value] -eq '}') {
                $Index.Value += 1
                return
            }
            if ($Text[$Index.Value] -ne ',') { throw 'JSON object entries must be comma separated.' }
            $Index.Value += 1
        }
    }
    if ($character -eq '[') {
        $Index.Value += 1
        Skip-PusulaJsonWhitespace -Text $Text -Index $Index
        if ($Index.Value -lt $Text.Length -and $Text[$Index.Value] -eq ']') {
            $Index.Value += 1
            return
        }
        while ($true) {
            Read-PusulaJsonValue -Text $Text -Index $Index
            Skip-PusulaJsonWhitespace -Text $Text -Index $Index
            if ($Index.Value -ge $Text.Length) { throw 'JSON array is unterminated.' }
            if ($Text[$Index.Value] -eq ']') {
                $Index.Value += 1
                return
            }
            if ($Text[$Index.Value] -ne ',') { throw 'JSON array entries must be comma separated.' }
            $Index.Value += 1
        }
    }

    $remaining = $Text.Substring($Index.Value)
    foreach ($literal in 'true', 'false', 'null') {
        if ($remaining.StartsWith($literal, [StringComparison]::Ordinal)) {
            $Index.Value += $literal.Length
            return
        }
    }
    $number = [regex]::Match($remaining, '^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?')
    if (-not $number.Success) { throw "Invalid JSON value at offset $($Index.Value)." }
    $Index.Value += $number.Length
}

function Assert-PusulaJsonSyntaxAndUniqueProperties {
    param([Parameter(Mandatory = $true)][string] $Text)
    $index = 0
    Read-PusulaJsonValue -Text $Text -Index ([ref]$index)
    Skip-PusulaJsonWhitespace -Text $Text -Index ([ref]$index)
    if ($index -ne $Text.Length) { throw "Unexpected JSON content at offset $index." }
}

function Assert-PusulaExactProperties {
    param(
        [Parameter(Mandatory = $true)] $Value,
        [Parameter(Mandatory = $true)][string[]] $Expected,
        [Parameter(Mandatory = $true)][string] $Path
    )
    if ($null -eq $Value -or $Value -isnot [pscustomobject]) { throw "$Path must be a JSON object." }
    $actual = @($Value.PSObject.Properties.Name | Sort-Object)
    $sortedExpected = @($Expected | Sort-Object)
    if (($actual -join "`n") -cne ($sortedExpected -join "`n")) {
        throw "$Path properties must exactly equal: $($sortedExpected -join ', ')."
    }
}

function Get-PusulaRequiredString {
    param($Value, [string] $Path, [string] $Pattern = '')
    if ($Value -isnot [string] -or [string]::IsNullOrWhiteSpace([string]$Value)) {
        throw "$Path must be a non-empty string."
    }
    $text = [string]$Value
    if ($Pattern -and $text -cnotmatch $Pattern) { throw "$Path has an invalid format." }
    return $text
}

function Get-PusulaBoundedNonControlString {
    param($Value, [string] $Path, [int] $MaximumLength = 256)
    $text = Get-PusulaRequiredString -Value $Value -Path $Path
    if ($MaximumLength -le 0 -or $text.Length -gt $MaximumLength) {
        throw "$Path must contain at most $MaximumLength characters."
    }
    foreach ($character in $text.ToCharArray()) {
        if ([char]::IsControl($character)) { throw "$Path must not contain control characters." }
    }
    return $text
}

function Get-PusulaSha256 {
    param($Value, [string] $Path)
    return (Get-PusulaRequiredString -Value $Value -Path $Path -Pattern '^[0-9a-f]{64}$')
}

function Get-PusulaNonNegativeInt64 {
    param($Value, [string] $Path, [switch] $Positive)
    $integerTypes = @(
        [byte], [sbyte], [int16], [uint16], [int32], [uint32], [int64], [uint64]
    )
    if ($null -eq $Value -or $Value.GetType() -notin $integerTypes) {
        throw "$Path must be an exact JSON integer."
    }
    $parsed = 0L
    if (-not [long]::TryParse(
            [string]$Value,
            [Globalization.NumberStyles]::None,
            [Globalization.CultureInfo]::InvariantCulture,
            [ref]$parsed)) {
        throw "$Path must be a 64-bit integer."
    }
    if ($parsed -lt 0 -or ($Positive -and $parsed -le 0)) { throw "$Path is outside the allowed range." }
    return $parsed
}

function Assert-PusulaTrue {
    param($Value, [string] $Path)
    if ($Value -isnot [bool] -or -not [bool]$Value) { throw "$Path must be the JSON boolean true." }
}

function Get-PusulaSummary {
    param($Value, [string] $Path)
    Assert-PusulaExactProperties -Value $Value -Expected @('counts', 'totals') -Path $Path
    Assert-PusulaExactProperties -Value $Value.counts `
        -Expected @('customers', 'contacts', 'sales', 'installments', 'payments') `
        -Path "$Path.counts"
    Assert-PusulaExactProperties -Value $Value.totals `
        -Expected @('sales_kurus', 'installments_kurus', 'payments_kurus') `
        -Path "$Path.totals"
    return [ordered]@{
        counts = [ordered]@{
            customers = Get-PusulaNonNegativeInt64 $Value.counts.customers "$Path.counts.customers"
            contacts = Get-PusulaNonNegativeInt64 $Value.counts.contacts "$Path.counts.contacts"
            sales = Get-PusulaNonNegativeInt64 $Value.counts.sales "$Path.counts.sales"
            installments = Get-PusulaNonNegativeInt64 $Value.counts.installments "$Path.counts.installments"
            payments = Get-PusulaNonNegativeInt64 $Value.counts.payments "$Path.counts.payments"
        }
        totals = [ordered]@{
            sales_kurus = Get-PusulaNonNegativeInt64 $Value.totals.sales_kurus "$Path.totals.sales_kurus"
            installments_kurus = Get-PusulaNonNegativeInt64 $Value.totals.installments_kurus "$Path.totals.installments_kurus"
            payments_kurus = Get-PusulaNonNegativeInt64 $Value.totals.payments_kurus "$Path.totals.payments_kurus"
        }
    }
}

function Assert-PusulaSummaryEquals {
    param($Actual, $Expected, [string] $Path)
    foreach ($name in 'customers', 'contacts', 'sales', 'installments', 'payments') {
        if ([long]$Actual.counts[$name] -ne [long]$Expected.counts[$name]) {
            throw "$Path.counts.$name does not match the committed acceptance fixture."
        }
    }
    foreach ($name in 'sales_kurus', 'installments_kurus', 'payments_kurus') {
        if ([long]$Actual.totals[$name] -ne [long]$Expected.totals[$name]) {
            throw "$Path.totals.$name does not match the committed acceptance fixture."
        }
    }
}

function Get-PusulaCanonicalAcceptanceEvidence {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] $Evidence,
        [Parameter(Mandatory = $true)][string] $Repository,
        [Parameter(Mandatory = $true)][string] $Version,
        [Parameter(Mandatory = $true)][string] $CandidateTag,
        [Parameter(Mandatory = $true)][string] $CandidateCommit,
        [Parameter(Mandatory = $true)][string] $CandidateAssetDirectory,
        [Parameter(Mandatory = $true)][string] $ExpectedWindowsPublisher,
        [Parameter(Mandatory = $true)][string] $ExpectedWindowsCertificateSha256,
        [Parameter(Mandatory = $true)][string] $FixturePath
    )

    Assert-PusulaExactProperties -Value $Evidence `
        -Expected @('schema_version', 'repository', 'version', 'candidate', 'acceptance') `
        -Path '$'
    if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' -or
        $Version -cnotmatch '^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$') {
        throw 'Expected acceptance repository or final SemVer is invalid.'
    }
    if ((Get-PusulaNonNegativeInt64 $Evidence.schema_version '$.schema_version') -ne 2) {
        throw '$.schema_version must equal 2.'
    }
    if ((Get-PusulaRequiredString $Evidence.repository '$.repository') -cne $Repository) {
        throw '$.repository does not match the release repository.'
    }
    if ((Get-PusulaRequiredString $Evidence.version '$.version') -cne $Version) {
        throw '$.version does not match the release version.'
    }
    if ($CandidateCommit -cnotmatch '^[0-9a-f]{40}$' -or
        $CandidateTag -cne "v$Version-candidate.$CandidateCommit") {
        throw 'Expected candidate tag or commit is invalid.'
    }
    $expectedCertificate = $ExpectedWindowsCertificateSha256.ToLowerInvariant()
    if ($expectedCertificate -cnotmatch '^[0-9a-f]{64}$') {
        throw 'Expected Windows certificate SHA-256 is invalid.'
    }

    Assert-PusulaExactProperties -Value $Evidence.candidate `
        -Expected @('tag', 'commit', 'workflow_run_id', 'assets') `
        -Path '$.candidate'
    if ((Get-PusulaRequiredString $Evidence.candidate.tag '$.candidate.tag') -cne $CandidateTag -or
        (Get-PusulaRequiredString $Evidence.candidate.commit '$.candidate.commit' '^[0-9a-f]{40}$') -cne $CandidateCommit) {
        throw 'Acceptance evidence candidate tag or commit is not the promoted candidate.'
    }
    $workflowRunId = Get-PusulaNonNegativeInt64 $Evidence.candidate.workflow_run_id '$.candidate.workflow_run_id' -Positive

    $assetDirectory = (Resolve-Path -LiteralPath $CandidateAssetDirectory -ErrorAction Stop).Path
    $expectedNames = @(
        'latest.json',
        "Pusula_${Version}_x64_offline-setup.exe",
        "Pusula_${Version}_x64-setup.exe",
        "Pusula_${Version}_x64-setup.exe.sig",
        'SHA256SUMS.txt'
    )
    $assetRows = @($Evidence.candidate.assets)
    if ($assetRows.Count -ne $expectedNames.Count) { throw '$.candidate.assets must contain exactly five rows.' }
    $canonicalAssetMap = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([StringComparer]::Ordinal)
    foreach ($row in $assetRows) {
        Assert-PusulaExactProperties -Value $row -Expected @('name', 'size', 'sha256') -Path '$.candidate.assets[]'
        $name = Get-PusulaRequiredString $row.name '$.candidate.assets[].name' '^[A-Za-z0-9_.-]+$'
        $path = Join-Path $assetDirectory $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Evidence asset does not exist in the candidate: $name" }
        $file = Get-Item -LiteralPath $path
        $size = Get-PusulaNonNegativeInt64 $row.size "$.candidate.assets[$name].size" -Positive
        $hash = Get-PusulaSha256 $row.sha256 "$.candidate.assets[$name].sha256"
        if ($size -ne [long]$file.Length -or
            $hash -cne (Get-FileHash -Algorithm SHA256 -LiteralPath $file.FullName).Hash.ToLowerInvariant()) {
            throw "Acceptance evidence asset bytes differ from the immutable candidate: $name"
        }
        if ($canonicalAssetMap.ContainsKey($name)) { throw "Acceptance evidence has a duplicate candidate asset: $name" }
        $canonicalAssetMap.Add($name, [pscustomobject][ordered]@{ name = $name; size = $size; sha256 = $hash })
    }
    $canonicalAssets = @()
    foreach ($name in $expectedNames) {
        if (-not $canonicalAssetMap.ContainsKey($name)) {
            throw '$.candidate.assets names do not equal the exact release asset allowlist.'
        }
        $canonicalAssets += $canonicalAssetMap[$name]
    }

    Assert-PusulaExactProperties -Value $Evidence.acceptance `
        -Expected @(
            'started_at_utc', 'completed_at_utc', 'windows', 'baseline',
            'candidate_install', 'fixture_restore', 'backup', 'invalid_signature', 'checks'
        ) `
        -Path '$.acceptance'
    $timestampPattern = '^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{7}Z$'
    $startedText = Get-PusulaRequiredString $Evidence.acceptance.started_at_utc '$.acceptance.started_at_utc' $timestampPattern
    $completedText = Get-PusulaRequiredString $Evidence.acceptance.completed_at_utc '$.acceptance.completed_at_utc' $timestampPattern
    $started = [DateTimeOffset]::ParseExact($startedText, 'yyyy-MM-ddTHH:mm:ss.fffffffZ', [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::AssumeUniversal)
    $completed = [DateTimeOffset]::ParseExact($completedText, 'yyyy-MM-ddTHH:mm:ss.fffffffZ', [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::AssumeUniversal)
    if ($completed -lt $started -or $completed -gt [DateTimeOffset]::UtcNow.AddMinutes(5)) {
        throw 'Acceptance evidence timestamps are not a completed UTC interval.'
    }

    Assert-PusulaExactProperties -Value $Evidence.acceptance.windows `
        -Expected @(
            'version', 'architecture', 'standard_user', 'clean_profile',
            'network_disconnected', 'offline_install', 'restart_completed'
        ) `
        -Path '$.acceptance.windows'
    $windowsVersion = Get-PusulaRequiredString $Evidence.acceptance.windows.version '$.acceptance.windows.version' '^10\.0\.[0-9]{4,6}(?:\.[0-9]+)?$'
    if ((Get-PusulaRequiredString $Evidence.acceptance.windows.architecture '$.acceptance.windows.architecture') -cne 'windows-x86_64') {
        throw '$.acceptance.windows.architecture must equal windows-x86_64.'
    }
    foreach ($name in 'standard_user', 'clean_profile', 'network_disconnected', 'offline_install', 'restart_completed') {
        Assert-PusulaTrue $Evidence.acceptance.windows.$name "$.acceptance.windows.$name"
    }

    Assert-PusulaExactProperties -Value $Evidence.acceptance.baseline `
        -Expected @(
            'version', 'archive_sha256', 'installed_exe_sha256', 'publisher',
            'certificate_sha256', 'authenticode_valid', 'timestamped'
        ) `
        -Path '$.acceptance.baseline'
    if ((Get-PusulaRequiredString $Evidence.acceptance.baseline.version '$.acceptance.baseline.version') -cne '0.0.9') {
        throw '$.acceptance.baseline.version must equal 0.0.9.'
    }
    $baselineArchiveHash = Get-PusulaSha256 $Evidence.acceptance.baseline.archive_sha256 '$.acceptance.baseline.archive_sha256'
    $baselineExecutableHash = Get-PusulaSha256 $Evidence.acceptance.baseline.installed_exe_sha256 '$.acceptance.baseline.installed_exe_sha256'
    $baselinePublisher = Get-PusulaRequiredString $Evidence.acceptance.baseline.publisher '$.acceptance.baseline.publisher' '^[A-Za-z0-9][A-Za-z0-9 .,&()''-]{1,127}$'
    $baselineCertificate = Get-PusulaSha256 $Evidence.acceptance.baseline.certificate_sha256 '$.acceptance.baseline.certificate_sha256'
    if ($baselinePublisher -cne $ExpectedWindowsPublisher -or $baselineCertificate -cne $expectedCertificate) {
        throw 'Baseline publisher or certificate does not match the protected release configuration.'
    }
    Assert-PusulaTrue $Evidence.acceptance.baseline.authenticode_valid '$.acceptance.baseline.authenticode_valid'
    Assert-PusulaTrue $Evidence.acceptance.baseline.timestamped '$.acceptance.baseline.timestamped'

    Assert-PusulaExactProperties -Value $Evidence.acceptance.candidate_install `
        -Expected @(
            'version', 'installed_exe_sha256', 'publisher', 'certificate_sha256',
            'authenticode_valid', 'timestamped'
        ) `
        -Path '$.acceptance.candidate_install'
    if ((Get-PusulaRequiredString $Evidence.acceptance.candidate_install.version '$.acceptance.candidate_install.version') -cne $Version) {
        throw '$.acceptance.candidate_install.version does not match the promoted version.'
    }
    $candidateExecutableHash = Get-PusulaSha256 $Evidence.acceptance.candidate_install.installed_exe_sha256 '$.acceptance.candidate_install.installed_exe_sha256'
    $candidatePublisher = Get-PusulaRequiredString $Evidence.acceptance.candidate_install.publisher '$.acceptance.candidate_install.publisher' '^[A-Za-z0-9][A-Za-z0-9 .,&()''-]{1,127}$'
    $candidateCertificate = Get-PusulaSha256 $Evidence.acceptance.candidate_install.certificate_sha256 '$.acceptance.candidate_install.certificate_sha256'
    if ($candidatePublisher -cne $ExpectedWindowsPublisher -or $candidatePublisher -cne $baselinePublisher -or
        $candidateCertificate -cne $expectedCertificate -or $candidateCertificate -cne $baselineCertificate) {
        throw 'Baseline and candidate publisher/certificate identities must exactly match the protected configuration.'
    }
    Assert-PusulaTrue $Evidence.acceptance.candidate_install.authenticode_valid '$.acceptance.candidate_install.authenticode_valid'
    Assert-PusulaTrue $Evidence.acceptance.candidate_install.timestamped '$.acceptance.candidate_install.timestamped'

    Assert-PusulaExactProperties -Value $Evidence.acceptance.fixture_restore `
        -Expected @('fixture_manifest_sha256', 'source', 'restored') `
        -Path '$.acceptance.fixture_restore'
    $fixture = (Resolve-Path -LiteralPath $FixturePath -ErrorAction Stop).Path
    $fixtureJson = Get-Content -Raw -LiteralPath $fixture | ConvertFrom-Json
    $fixtureManifestHash = Get-PusulaSha256 $fixtureJson.manifest.sha256 '$fixture.manifest.sha256'
    $evidenceManifestHash = Get-PusulaSha256 `
        $Evidence.acceptance.fixture_restore.fixture_manifest_sha256 `
        '$.acceptance.fixture_restore.fixture_manifest_sha256'
    if ($fixtureManifestHash -cne $script:PusulaAcceptanceFixtureManifestSha256 -or
        $evidenceManifestHash -cne $script:PusulaAcceptanceFixtureManifestSha256) {
        throw 'Acceptance evidence is not bound to the committed logical Pusula fixture manifest.'
    }
    $expectedFixtureSummary = [ordered]@{
        counts = [ordered]@{ customers = 2L; contacts = 1L; sales = 2L; installments = 1L; payments = 1L }
        totals = [ordered]@{ sales_kurus = 1234567900L; installments_kurus = 5L; payments_kurus = 1L }
    }
    $fixtureManifestSummary = Get-PusulaSummary ([pscustomobject][ordered]@{
            counts = $fixtureJson.manifest.counts
            totals = $fixtureJson.manifest.totals
        }) '$fixture.manifest'
    Assert-PusulaSummaryEquals $fixtureManifestSummary $expectedFixtureSummary '$fixture.manifest'
    $sourceSummary = Get-PusulaSummary $Evidence.acceptance.fixture_restore.source '$.acceptance.fixture_restore.source'
    $restoredSummary = Get-PusulaSummary $Evidence.acceptance.fixture_restore.restored '$.acceptance.fixture_restore.restored'
    Assert-PusulaSummaryEquals $sourceSummary $expectedFixtureSummary '$.acceptance.fixture_restore.source'
    Assert-PusulaSummaryEquals $restoredSummary $expectedFixtureSummary '$.acceptance.fixture_restore.restored'

    Assert-PusulaExactProperties -Value $Evidence.acceptance.backup `
        -Expected @(
            'backup_id', 'ciphertext_sha256', 'desktop_size', 'gateway_sha256',
            'gateway_size', 'gateway_version_id', 'gateway_verified_at_utc',
            'b2_sha256', 'b2_size', 'b2_version_id', 'gateway_spool_empty',
            'sqlite_integrity', 'foreign_keys'
        ) `
        -Path '$.acceptance.backup'
    $backupId = Get-PusulaRequiredString $Evidence.acceptance.backup.backup_id '$.acceptance.backup.backup_id' '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'
    $ciphertextHash = Get-PusulaSha256 $Evidence.acceptance.backup.ciphertext_sha256 '$.acceptance.backup.ciphertext_sha256'
    $gatewayHash = Get-PusulaSha256 $Evidence.acceptance.backup.gateway_sha256 '$.acceptance.backup.gateway_sha256'
    $b2Hash = Get-PusulaSha256 $Evidence.acceptance.backup.b2_sha256 '$.acceptance.backup.b2_sha256'
    $desktopSize = Get-PusulaNonNegativeInt64 $Evidence.acceptance.backup.desktop_size '$.acceptance.backup.desktop_size' -Positive
    $gatewaySize = Get-PusulaNonNegativeInt64 $Evidence.acceptance.backup.gateway_size '$.acceptance.backup.gateway_size' -Positive
    $b2Size = Get-PusulaNonNegativeInt64 $Evidence.acceptance.backup.b2_size '$.acceptance.backup.b2_size' -Positive
    if ($ciphertextHash -cne $gatewayHash -or $ciphertextHash -cne $b2Hash -or
        $desktopSize -ne $gatewaySize -or $desktopSize -ne $b2Size) {
        throw 'Desktop, gateway, and B2 ciphertext size/hash evidence must exactly match.'
    }
    $gatewayVersionId = Get-PusulaBoundedNonControlString `
        $Evidence.acceptance.backup.gateway_version_id '$.acceptance.backup.gateway_version_id'
    $b2VersionId = Get-PusulaBoundedNonControlString `
        $Evidence.acceptance.backup.b2_version_id '$.acceptance.backup.b2_version_id'
    if ($gatewayVersionId -cne $b2VersionId) {
        throw 'Gateway and B2 version IDs must exactly match.'
    }
    $gatewayVerifiedAtText = Get-PusulaRequiredString `
        $Evidence.acceptance.backup.gateway_verified_at_utc `
        '$.acceptance.backup.gateway_verified_at_utc' `
        $timestampPattern
    $gatewayVerifiedAt = [DateTimeOffset]::ParseExact(
        $gatewayVerifiedAtText,
        'yyyy-MM-ddTHH:mm:ss.fffffffZ',
        [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::AssumeUniversal
    )
    if ($gatewayVerifiedAt -lt $started -or $gatewayVerifiedAt -gt $completed) {
        throw 'Gateway verification time must fall within the completed acceptance interval.'
    }
    Assert-PusulaTrue $Evidence.acceptance.backup.gateway_spool_empty '$.acceptance.backup.gateway_spool_empty'
    if ((Get-PusulaRequiredString $Evidence.acceptance.backup.sqlite_integrity '$.acceptance.backup.sqlite_integrity') -cne 'ok' -or
        (Get-PusulaRequiredString $Evidence.acceptance.backup.foreign_keys '$.acceptance.backup.foreign_keys') -cne 'ok') {
        throw 'Restored SQLite integrity and foreign-key results must both equal ok.'
    }

    Assert-PusulaExactProperties -Value $Evidence.acceptance.invalid_signature `
        -Expected @(
            'evidence_sha256', 'result', 'source_commit', 'candidate_sha256',
            'signature_sha256', 'candidate_unchanged', 'signature_unchanged',
            'original_signature_verification', 'tampered_copy_signature_verification',
            'runtime_rejection_phase', 'installation_confirmation_called',
            'dangerous_updater_overrides', 'production_configuration_modified',
            'installer_created_or_run'
        ) `
        -Path '$.acceptance.invalid_signature'
    $invalidEvidenceHash = Get-PusulaSha256 $Evidence.acceptance.invalid_signature.evidence_sha256 '$.acceptance.invalid_signature.evidence_sha256'
    if ((Get-PusulaRequiredString $Evidence.acceptance.invalid_signature.result '$.acceptance.invalid_signature.result') -cne 'pass' -or
        (Get-PusulaRequiredString $Evidence.acceptance.invalid_signature.source_commit '$.acceptance.invalid_signature.source_commit' '^[0-9a-f]{40}$') -cne $CandidateCommit) {
        throw 'Invalid-signature evidence is not a pass from the candidate source commit.'
    }
    $leanAsset = @($canonicalAssets | Where-Object { $_.name -ceq "Pusula_${Version}_x64-setup.exe" })[0]
    $signatureAsset = @($canonicalAssets | Where-Object { $_.name -ceq "Pusula_${Version}_x64-setup.exe.sig" })[0]
    if ((Get-PusulaSha256 $Evidence.acceptance.invalid_signature.candidate_sha256 '$.acceptance.invalid_signature.candidate_sha256') -cne $leanAsset.sha256 -or
        (Get-PusulaSha256 $Evidence.acceptance.invalid_signature.signature_sha256 '$.acceptance.invalid_signature.signature_sha256') -cne $signatureAsset.sha256) {
        throw 'Invalid-signature evidence is not bound to the exact candidate updater and signature assets.'
    }
    Assert-PusulaTrue $Evidence.acceptance.invalid_signature.candidate_unchanged '$.acceptance.invalid_signature.candidate_unchanged'
    Assert-PusulaTrue $Evidence.acceptance.invalid_signature.signature_unchanged '$.acceptance.invalid_signature.signature_unchanged'
    if ((Get-PusulaRequiredString $Evidence.acceptance.invalid_signature.original_signature_verification '$.acceptance.invalid_signature.original_signature_verification') -cne 'accepted' -or
        (Get-PusulaRequiredString $Evidence.acceptance.invalid_signature.tampered_copy_signature_verification '$.acceptance.invalid_signature.tampered_copy_signature_verification') -cne 'rejected' -or
        (Get-PusulaRequiredString $Evidence.acceptance.invalid_signature.runtime_rejection_phase '$.acceptance.invalid_signature.runtime_rejection_phase') -cne 'downloading') {
        throw 'Invalid-signature runtime evidence does not describe the required rejection path.'
    }
    foreach ($name in 'installation_confirmation_called', 'dangerous_updater_overrides', 'production_configuration_modified', 'installer_created_or_run') {
        if ($Evidence.acceptance.invalid_signature.$name -isnot [bool] -or [bool]$Evidence.acceptance.invalid_signature.$name) {
            throw "$.acceptance.invalid_signature.$name must be the JSON boolean false."
        }
    }

    Assert-PusulaExactProperties -Value $Evidence.acceptance.checks -Expected $script:PusulaAcceptanceCheckNames -Path '$.acceptance.checks'
    $canonicalChecks = [ordered]@{}
    foreach ($name in $script:PusulaAcceptanceCheckNames) {
        if ((Get-PusulaRequiredString $Evidence.acceptance.checks.$name "$.acceptance.checks.$name") -cne 'pass') {
            throw "$.acceptance.checks.$name must equal pass."
        }
        $canonicalChecks[$name] = 'pass'
    }

    return [ordered]@{
        schema_version = 2
        repository = $Repository
        version = $Version
        candidate = [ordered]@{
            tag = $CandidateTag
            commit = $CandidateCommit
            workflow_run_id = $workflowRunId
            assets = @($canonicalAssets)
        }
        acceptance = [ordered]@{
            started_at_utc = $startedText
            completed_at_utc = $completedText
            windows = [ordered]@{
                version = $windowsVersion
                architecture = 'windows-x86_64'
                standard_user = $true
                clean_profile = $true
                network_disconnected = $true
                offline_install = $true
                restart_completed = $true
            }
            baseline = [ordered]@{
                version = '0.0.9'
                archive_sha256 = $baselineArchiveHash
                installed_exe_sha256 = $baselineExecutableHash
                publisher = $baselinePublisher
                certificate_sha256 = $baselineCertificate
                authenticode_valid = $true
                timestamped = $true
            }
            candidate_install = [ordered]@{
                version = $Version
                installed_exe_sha256 = $candidateExecutableHash
                publisher = $candidatePublisher
                certificate_sha256 = $candidateCertificate
                authenticode_valid = $true
                timestamped = $true
            }
            fixture_restore = [ordered]@{
                fixture_manifest_sha256 = $fixtureManifestHash
                source = $sourceSummary
                restored = $restoredSummary
            }
            backup = [ordered]@{
                backup_id = $backupId
                ciphertext_sha256 = $ciphertextHash
                desktop_size = $desktopSize
                gateway_sha256 = $gatewayHash
                gateway_size = $gatewaySize
                gateway_version_id = $gatewayVersionId
                gateway_verified_at_utc = $gatewayVerifiedAtText
                b2_sha256 = $b2Hash
                b2_size = $b2Size
                b2_version_id = $b2VersionId
                gateway_spool_empty = $true
                sqlite_integrity = 'ok'
                foreign_keys = 'ok'
            }
            invalid_signature = [ordered]@{
                evidence_sha256 = $invalidEvidenceHash
                result = 'pass'
                source_commit = $CandidateCommit
                candidate_sha256 = $leanAsset.sha256
                signature_sha256 = $signatureAsset.sha256
                candidate_unchanged = $true
                signature_unchanged = $true
                original_signature_verification = 'accepted'
                tampered_copy_signature_verification = 'rejected'
                runtime_rejection_phase = 'downloading'
                installation_confirmation_called = $false
                dangerous_updater_overrides = $false
                production_configuration_modified = $false
                installer_created_or_run = $false
            }
            checks = $canonicalChecks
        }
    }
}

function ConvertTo-PusulaCanonicalJson {
    param([Parameter(Mandatory = $true)] $Value)
    return ($Value | ConvertTo-Json -Depth 16 -Compress)
}

function Read-PusulaStrictUtf8File {
    param([Parameter(Mandatory = $true)][string] $Path)
    $resolved = (Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path
    $bytes = [IO.File]::ReadAllBytes($resolved)
    if ($bytes.Length -le 0 -or $bytes.Length -gt $script:PusulaAcceptanceEvidenceMaximumBytes) {
        throw "Acceptance evidence must be between 1 and $script:PusulaAcceptanceEvidenceMaximumBytes bytes."
    }
    if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
        throw 'Acceptance evidence must be UTF-8 without a BOM.'
    }
    $utf8 = New-Object Text.UTF8Encoding($false, $true)
    try { $text = $utf8.GetString($bytes) }
    catch { throw 'Acceptance evidence is not valid strict UTF-8.' }
    return [pscustomobject][ordered]@{ path = $resolved; bytes = $bytes; text = $text }
}

function ConvertFrom-PusulaAcceptanceEvidenceBase64 {
    param([Parameter(Mandatory = $true)][string] $Base64)

    if ($Base64.Length -le 0 -or $Base64.Length -gt $script:PusulaAcceptanceEvidenceMaximumBase64Characters) {
        throw "Acceptance evidence base64 must be between 1 and $script:PusulaAcceptanceEvidenceMaximumBase64Characters characters."
    }
    if (($Base64.Length % 4) -ne 0 -or
        $Base64 -cnotmatch '^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$') {
        throw 'Acceptance evidence must use strict standard base64 without whitespace.'
    }
    try { $bytes = [Convert]::FromBase64String($Base64) }
    catch { throw 'Acceptance evidence base64 could not be decoded.' }
    if ($bytes.Length -le 0 -or $bytes.Length -gt $script:PusulaAcceptanceEvidenceMaximumBytes) {
        throw "Decoded acceptance evidence must be between 1 and $script:PusulaAcceptanceEvidenceMaximumBytes bytes."
    }
    $roundTrip = [Convert]::ToBase64String($bytes)
    if (-not [string]::Equals($roundTrip, $Base64, [StringComparison]::Ordinal)) {
        throw 'Acceptance evidence base64 is not the exact canonical encoding of its decoded bytes.'
    }
    if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
        throw 'Acceptance evidence must be UTF-8 without a BOM.'
    }
    $utf8 = New-Object Text.UTF8Encoding($false, $true)
    try { $text = $utf8.GetString($bytes) }
    catch { throw 'Acceptance evidence is not valid strict UTF-8.' }
    return [pscustomobject][ordered]@{ bytes = $bytes; text = $text }
}
