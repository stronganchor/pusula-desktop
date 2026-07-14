#Requires -Version 5.1

<#
.SYNOPSIS
Validates or restores an encrypted Pusula SQLite backup on Windows.

.DESCRIPTION
The default operation decrypts into a private, non-OneDrive staging directory,
validates the Pusula schema and data invariants, prints a JSON evidence report,
and removes the staging directory. Use -Apply to replace the live
database. An applied restore requires an independent evidence JSON file and
creates a verified, timestamped rollback before replacement.

.PARAMETER CiphertextPath
Path to the age-encrypted Pusula backup.

.PARAMETER RecoveryIdentityPath
Path to the age X25519 recovery identity. The script never copies or prints it.

.PARAMETER ExpectedEvidencePath
Path to a prior incident/recovery evidence JSON document. Required with -Apply.

.PARAMETER Apply
Replace the target database after all validation gates pass. Without this
switch, the script only validates and reports.

.EXAMPLE
./scripts/Restore-PusulaBackup.ps1 `
  -CiphertextPath 'C:\secure\backup.sqlite3.age' `
  -RecoveryIdentityPath 'E:\keys\pusula-recovery.agekey'

.EXAMPLE
./scripts/Restore-PusulaBackup.ps1 `
  -CiphertextPath 'C:\secure\backup.sqlite3.age' `
  -RecoveryIdentityPath 'E:\keys\pusula-recovery.agekey' `
  -ExpectedEvidencePath 'C:\secure\restore-evidence.json' `
  -Apply
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$CiphertextPath,

    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$RecoveryIdentityPath,

    [string]$ExpectedEvidencePath,

    [string]$TargetDatabasePath,

    [string]$StagingRoot,

    [string]$RollbackRoot,

    [string]$RagePath = 'rage.exe',

    [string]$PythonPath = 'python.exe',

    [switch]$Apply
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:SupportedSchemaVersion = 1
$script:StagingDirectory = $null
$script:CandidatePath = $null
$script:LockPath = $null
$script:LockStream = $null
$script:CleanupFailure = $null
$script:RollbackDirectory = $null
$script:KeepRollback = $false

$script:SqliteHelper = @'
import json
import pathlib
import sqlite3
import sys
import uuid

EXPECTED_TABLE_COLUMNS = {
    "business_profile": {
        "id", "name", "address", "phone", "website", "footer_sub"
    },
    "customers": {
        "id", "name", "phone", "address", "work_address", "notes",
        "registration_date"
    },
    "contacts": {
        "id", "customer_id", "name", "phone", "home_address", "work_address"
    },
    "sales": {
        "id", "customer_id", "date", "total_kurus", "description",
        "request_key"
    },
    "installments": {
        "id", "sale_id", "due_date", "amount_kurus", "paid_date"
    },
    "installment_payments": {
        "id", "installment_id", "amount_kurus", "payment_date", "created_at"
    },
    "settings": {"key", "value"},
}

COUNT_TABLES = {
    "customers": "customers",
    "contacts": "contacts",
    "sales": "sales",
    "installments": "installments",
    "payments": "installment_payments",
}

TOTAL_QUERIES = {
    "sales_kurus": "SELECT COALESCE(SUM(total_kurus), 0) FROM sales",
    "installments_kurus": "SELECT COALESCE(SUM(amount_kurus), 0) FROM installments",
    "payments_kurus": "SELECT COALESCE(SUM(amount_kurus), 0) FROM installment_payments",
}


def readonly_uri(path, immutable):
    uri = pathlib.Path(path).resolve().as_uri() + "?mode=ro"
    if immutable:
        uri += "&immutable=1"
    return uri


def scalar(connection, sql):
    row = connection.execute(sql).fetchone()
    if row is None:
        raise ValueError("required query returned no row")
    return row[0]


def validate_database(path, expected_schema):
    connection = sqlite3.connect(
        readonly_uri(path, immutable=True), uri=True, timeout=5.0
    )
    try:
        connection.execute("PRAGMA query_only = ON")
        connection.execute("PRAGMA trusted_schema = OFF")

        integrity_rows = [row[0] for row in connection.execute("PRAGMA integrity_check")]
        if integrity_rows != ["ok"]:
            raise ValueError("PRAGMA integrity_check did not return exactly 'ok'")

        foreign_key_rows = list(connection.execute("PRAGMA foreign_key_check"))
        if foreign_key_rows:
            raise ValueError("PRAGMA foreign_key_check reported violations")

        user_version = int(scalar(connection, "PRAGMA user_version"))
        if user_version != expected_schema:
            raise ValueError(
                f"unsupported Pusula user_version {user_version}; expected {expected_schema}"
            )

        table_rows = connection.execute(
            "SELECT name FROM sqlite_schema WHERE type = 'table'"
        ).fetchall()
        tables = {row[0] for row in table_rows}
        missing_tables = sorted(set(EXPECTED_TABLE_COLUMNS) - tables)
        if missing_tables:
            raise ValueError("missing required Pusula tables: " + ", ".join(missing_tables))

        for table, required_columns in EXPECTED_TABLE_COLUMNS.items():
            columns = {row[1] for row in connection.execute(f'PRAGMA table_info("{table}")')}
            missing_columns = sorted(required_columns - columns)
            if missing_columns:
                raise ValueError(
                    f"table {table} is missing columns: " + ", ".join(missing_columns)
                )

        if int(scalar(connection, "SELECT COUNT(*) FROM business_profile WHERE id = 1")) != 1:
            raise ValueError("business_profile must contain exactly the id=1 row")

        settings = dict(connection.execute("SELECT key, value FROM settings"))
        database_id = settings.get("database_id")
        try:
            parsed_database_id = uuid.UUID(database_id)
        except (AttributeError, TypeError, ValueError):
            raise ValueError("settings.database_id is missing or is not a UUID")
        if str(parsed_database_id) != database_id.lower():
            raise ValueError("settings.database_id is not a canonical UUID")
        if settings.get("onboarding_complete") not in {"true", "false"}:
            raise ValueError("settings.onboarding_complete must be true or false")

        invalid_money = {
            "sales": int(scalar(connection,
                "SELECT COUNT(*) FROM sales "
                "WHERE typeof(total_kurus) <> 'integer' OR total_kurus < 0")),
            "installments": int(scalar(connection,
                "SELECT COUNT(*) FROM installments "
                "WHERE typeof(amount_kurus) <> 'integer' OR amount_kurus < 0")),
            "payments": int(scalar(connection,
                "SELECT COUNT(*) FROM installment_payments "
                "WHERE typeof(amount_kurus) <> 'integer' OR amount_kurus <= 0")),
        }
        if any(invalid_money.values()):
            raise ValueError("one or more money values violate Pusula integer-kurus rules")

        counts = {
            name: int(scalar(connection, f'SELECT COUNT(*) FROM "{table}"'))
            for name, table in COUNT_TABLES.items()
        }
        totals = {
            name: int(scalar(connection, sql))
            for name, sql in TOTAL_QUERIES.items()
        }
        if any(value < 0 for value in counts.values()) or any(
            value < 0 for value in totals.values()
        ):
            raise ValueError("negative count or financial total")

        return {
            "schema_version": user_version,
            "integrity_check": "ok",
            "foreign_key_violations": 0,
            "counts": counts,
            "totals": totals,
        }
    finally:
        connection.close()


def backup_database(source_path, destination_path):
    destination = pathlib.Path(destination_path)
    if destination.exists():
        raise ValueError("rollback destination already exists")

    source = sqlite3.connect(
        readonly_uri(source_path, immutable=False), uri=True, timeout=5.0
    )
    target = sqlite3.connect(str(destination), timeout=5.0)
    try:
        source.execute("PRAGMA busy_timeout = 5000")
        source.backup(target, pages=256, sleep=0.05)
        target.commit()
    finally:
        target.close()
        source.close()


def main():
    if len(sys.argv) < 2:
        raise ValueError("missing helper operation")

    operation = sys.argv[1]
    if operation == "validate" and len(sys.argv) == 4:
        result = validate_database(sys.argv[2], int(sys.argv[3]))
        print(json.dumps(result, separators=(",", ":"), sort_keys=True))
        return
    if operation == "backup" and len(sys.argv) == 4:
        backup_database(sys.argv[2], sys.argv[3])
        return
    raise ValueError("invalid helper operation")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"SQLite helper failed: {error}", file=sys.stderr)
        sys.exit(2)
'@

function Get-FullPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    return [System.IO.Path]::GetFullPath($Path)
}

function Resolve-ExistingFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Description
    )

    $fullPath = Get-FullPath -Path $Path
    if (-not [System.IO.File]::Exists($fullPath)) {
        throw "$Description was not found."
    }
    return $fullPath
}

function Resolve-Executable {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Description
    )

    if ([System.IO.File]::Exists($Path)) {
        return (Get-FullPath -Path $Path)
    }

    $command = Get-Command -Name $Path -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandType -in @('Application', 'ExternalScript') } |
        Select-Object -First 1
    if ($null -eq $command) {
        throw "$Description was not found."
    }
    return $command.Source
}

function Test-PathWithin {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Root
    )

    $fullPath = (Get-FullPath -Path $Path).TrimEnd('\')
    $fullRoot = (Get-FullPath -Path $Root).TrimEnd('\')
    if ([string]::Equals($fullPath, $fullRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $true
    }
    return $fullPath.StartsWith(
        $fullRoot + '\',
        [System.StringComparison]::OrdinalIgnoreCase
    )
}

function Assert-OutsideSyncRoots {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Description
    )

    $roots = @(
        $env:OneDrive,
        $env:OneDriveConsumer,
        $env:OneDriveCommercial
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Select-Object -Unique

    foreach ($root in $roots) {
        if (Test-PathWithin -Path $Path -Root $root) {
            throw "$Description must be outside every configured OneDrive root."
        }
    }
}

function Assert-NoReparsePoints {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Description
    )

    $current = Get-FullPath -Path $Path
    while (-not (Test-Path -LiteralPath $current)) {
        $parent = [System.IO.Path]::GetDirectoryName($current)
        if ([string]::IsNullOrWhiteSpace($parent) -or
            [string]::Equals($parent, $current, [System.StringComparison]::OrdinalIgnoreCase)) {
            break
        }
        $current = $parent
    }

    while (-not [string]::IsNullOrWhiteSpace($current) -and (Test-Path -LiteralPath $current)) {
        $item = Get-Item -LiteralPath $current -Force
        if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "$Description cannot use a symbolic link, junction, or other reparse point."
        }
        $parentInfo = [System.IO.Directory]::GetParent($item.FullName)
        if ($null -eq $parentInfo) {
            break
        }
        $current = $parentInfo.FullName
    }
}

function Assert-SameVolume {
    param(
        [Parameter(Mandatory = $true)][string]$FirstPath,
        [Parameter(Mandatory = $true)][string]$SecondPath
    )

    $firstRoot = [System.IO.Path]::GetPathRoot((Get-FullPath -Path $FirstPath))
    $secondRoot = [System.IO.Path]::GetPathRoot((Get-FullPath -Path $SecondPath))
    if (-not [string]::Equals($firstRoot, $secondRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw 'Target database and rollback root must be on the same Windows volume for atomic replacement.'
    }
}

function New-PrivateDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    [System.IO.Directory]::CreateDirectory($Path) | Out-Null
    $security = New-Object System.Security.AccessControl.DirectorySecurity
    $security.SetAccessRuleProtection($true, $false)
    $identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
    $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
        $identity,
        [System.Security.AccessControl.FileSystemRights]::FullControl,
        [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit',
        [System.Security.AccessControl.PropagationFlags]::None,
        [System.Security.AccessControl.AccessControlType]::Allow
    )
    $security.AddAccessRule($rule)
    [System.IO.Directory]::SetAccessControl($Path, $security)
}

function Assert-PusulaStopped {
    $running = @(Get-Process -Name 'pusula-desktop', 'pusula' -ErrorAction SilentlyContinue)
    if ($running.Count -gt 0) {
        $processIds = ($running | ForEach-Object { $_.Id } | Sort-Object -Unique) -join ', '
        throw "Pusula is running (process ID(s): $processIds). Close it before validation or restore."
    }
}

function Invoke-External {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$FailureMessage,
        [switch]$CaptureOutput
    )

    $quotedArguments = foreach ($argument in $Arguments) {
        if ($argument -notmatch '[\s"]') {
            $argument
            continue
        }

        # Apply the CommandLineToArgvW-compatible quoting rules used by native
        # Windows programs: escape quotes and double trailing backslashes
        # inside a quoted argument.
        $escaped = [regex]::Replace($argument, '(\\*)"', '$1$1\"')
        $escaped = [regex]::Replace($escaped, '(\\*)$', '$1$1')
        '"' + $escaped + '"'
    }

    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $Executable
    $startInfo.Arguments = $quotedArguments -join ' '
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "$FailureMessage (process did not start)."
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $output = $stdoutTask.Result
        # Drain stderr but intentionally never return or print it: rage can
        # include the recovery identity path in diagnostics.
        $null = $stderrTask.Result
        $exitCode = $process.ExitCode
    }
    finally {
        $process.Dispose()
    }
    if ($exitCode -ne 0) {
        throw "$FailureMessage (exit code $exitCode)."
    }
    if ($CaptureOutput) {
        return $output
    }
}

function Write-FileDurably {
    param([Parameter(Mandatory = $true)][string]$Path)

    $stream = [System.IO.File]::Open(
        $Path,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::ReadWrite,
        [System.IO.FileShare]::Read
    )
    try {
        $stream.Flush($true)
    }
    finally {
        $stream.Dispose()
    }
}

function Assert-SqliteHeader {
    param([Parameter(Mandatory = $true)][string]$Path)

    $expected = [System.Text.Encoding]::ASCII.GetBytes("SQLite format 3`0")
    $actual = New-Object byte[] $expected.Length
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        if ($stream.Read($actual, 0, $actual.Length) -ne $actual.Length) {
            throw 'Decrypted backup is too short to be a SQLite database.'
        }
    }
    finally {
        $stream.Dispose()
    }

    for ($index = 0; $index -lt $expected.Length; $index += 1) {
        if ($actual[$index] -ne $expected[$index]) {
            throw 'Decrypted backup does not have a valid SQLite header.'
        }
    }
}

function Invoke-SqliteHelper {
    param(
        [Parameter(Mandatory = $true)][string]$PythonExecutable,
        [Parameter(Mandatory = $true)][string]$HelperPath,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$FailureMessage,
        [switch]$CaptureOutput
    )

    return Invoke-External `
        -Executable $PythonExecutable `
        -Arguments (@('-I', $HelperPath) + $Arguments) `
        -FailureMessage $FailureMessage `
        -CaptureOutput:$CaptureOutput
}

function Test-DatabaseResultEqual {
    param(
        [Parameter(Mandatory = $true)]$Left,
        [Parameter(Mandatory = $true)]$Right
    )

    $leftJson = $Left | ConvertTo-Json -Depth 6 -Compress
    $rightJson = $Right | ConvertTo-Json -Depth 6 -Compress
    return [string]::Equals($leftJson, $rightJson, [System.StringComparison]::Ordinal)
}

function Test-PusulaDatabase {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$PythonExecutable,
        [Parameter(Mandatory = $true)][string]$HelperPath
    )

    Assert-SqliteHeader -Path $Path
    $json = Invoke-SqliteHelper `
        -PythonExecutable $PythonExecutable `
        -HelperPath $HelperPath `
        -Arguments @('validate', $Path, [string]$script:SupportedSchemaVersion) `
        -FailureMessage 'Pusula database validation failed' `
        -CaptureOutput
    try {
        return ($json | ConvertFrom-Json)
    }
    catch {
        throw 'Pusula database validator returned invalid JSON.'
    }
}

function Get-RequiredProperty {
    param(
        [Parameter(Mandatory = $true)]$Object,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Description
    )

    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property -or $null -eq $property.Value) {
        throw "Evidence is missing $Description."
    }
    return $property.Value
}

function ConvertTo-ExactNonNegativeInt64 {
    param(
        [Parameter(Mandatory = $true)]$Value,
        [Parameter(Mandatory = $true)][string]$Description
    )

    $text = [System.Convert]::ToString($Value, [System.Globalization.CultureInfo]::InvariantCulture)
    if ($text -notmatch '^[0-9]+$') {
        throw "Evidence $Description must be a nonnegative integer."
    }
    try {
        return [long]::Parse($text, [System.Globalization.CultureInfo]::InvariantCulture)
    }
    catch {
        throw "Evidence $Description is outside the supported 64-bit integer range."
    }
}

function Read-ExpectedEvidence {
    param([Parameter(Mandatory = $true)][string]$Path)

    $evidencePath = Resolve-ExistingFile -Path $Path -Description 'Expected evidence file'
    if ((Get-Item -LiteralPath $evidencePath).Length -gt 65536) {
        throw 'Expected evidence file is unexpectedly large.'
    }
    try {
        $raw = [System.IO.File]::ReadAllText($evidencePath)
        $evidence = $raw | ConvertFrom-Json
    }
    catch {
        throw 'Expected evidence file is not valid JSON.'
    }

    $schemaVersion = ConvertTo-ExactNonNegativeInt64 `
        -Value (Get-RequiredProperty $evidence 'schema_version' 'schema_version') `
        -Description 'schema_version'
    $ciphertextSha256 = [string](Get-RequiredProperty $evidence 'ciphertext_sha256' 'ciphertext_sha256')
    $ciphertextSize = ConvertTo-ExactNonNegativeInt64 `
        -Value (Get-RequiredProperty $evidence 'ciphertext_size_bytes' 'ciphertext_size_bytes') `
        -Description 'ciphertext_size_bytes'
    $counts = Get-RequiredProperty $evidence 'counts' 'counts'
    $totals = Get-RequiredProperty $evidence 'totals' 'totals'

    if ($schemaVersion -ne $script:SupportedSchemaVersion) {
        throw "Evidence schema_version must be $($script:SupportedSchemaVersion)."
    }
    if ($ciphertextSha256 -notmatch '^[A-Fa-f0-9]{64}$') {
        throw 'Evidence ciphertext_sha256 must be a 64-character hexadecimal SHA-256.'
    }
    if ($ciphertextSize -le 0) {
        throw 'Evidence ciphertext_size_bytes must be positive.'
    }

    $normalizedCounts = [ordered]@{}
    foreach ($name in @('customers', 'contacts', 'sales', 'installments', 'payments')) {
        $value = ConvertTo-ExactNonNegativeInt64 `
            -Value (Get-RequiredProperty $counts $name "counts.$name") `
            -Description "counts.$name"
        $normalizedCounts[$name] = $value
    }

    $normalizedTotals = [ordered]@{}
    foreach ($name in @('sales_kurus', 'installments_kurus', 'payments_kurus')) {
        $value = ConvertTo-ExactNonNegativeInt64 `
            -Value (Get-RequiredProperty $totals $name "totals.$name") `
            -Description "totals.$name"
        $normalizedTotals[$name] = $value
    }

    return [pscustomobject][ordered]@{
        schema_version = $schemaVersion
        ciphertext_sha256 = $ciphertextSha256.ToLowerInvariant()
        ciphertext_size_bytes = $ciphertextSize
        counts = [pscustomobject]$normalizedCounts
        totals = [pscustomobject]$normalizedTotals
    }
}

function Assert-EvidenceMatches {
    param(
        [Parameter(Mandatory = $true)]$Evidence,
        [Parameter(Mandatory = $true)]$DatabaseResult,
        [Parameter(Mandatory = $true)][string]$CiphertextSha256,
        [Parameter(Mandatory = $true)][long]$CiphertextSize
    )

    if (-not [string]::Equals(
        $Evidence.ciphertext_sha256,
        $CiphertextSha256.ToLowerInvariant(),
        [System.StringComparison]::Ordinal
    )) {
        throw 'Ciphertext SHA-256 does not match expected evidence.'
    }
    if ([long]$Evidence.ciphertext_size_bytes -ne $CiphertextSize) {
        throw 'Ciphertext size does not match expected evidence.'
    }
    if ([long]$Evidence.schema_version -ne [long]$DatabaseResult.schema_version) {
        throw 'Database schema version does not match expected evidence.'
    }

    foreach ($name in @('customers', 'contacts', 'sales', 'installments', 'payments')) {
        if ([long]$Evidence.counts.$name -ne [long]$DatabaseResult.counts.$name) {
            throw "Database count $name does not match expected evidence."
        }
    }
    foreach ($name in @('sales_kurus', 'installments_kurus', 'payments_kurus')) {
        if ([long]$Evidence.totals.$name -ne [long]$DatabaseResult.totals.$name) {
            throw "Database financial total $name does not match expected evidence."
        }
    }
}

function Assert-ExclusiveFileAccess {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not [System.IO.File]::Exists($Path)) {
        return
    }
    try {
        $probe = [System.IO.File]::Open(
            $Path,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None
        )
        $probe.Dispose()
    }
    catch {
        throw 'The live Pusula database or one of its journal files is in use. Close Pusula and retry.'
    }
}

function Remove-PlaintextPath {
    param([string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path -LiteralPath $Path)) {
        return
    }

    $lastError = $null
    for ($attempt = 0; $attempt -lt 10; $attempt += 1) {
        try {
            Remove-Item -LiteralPath $Path -Recurse -Force
            return
        }
        catch {
            $lastError = $_
            Start-Sleep -Milliseconds 100
        }
    }
    throw $lastError
}

if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
    throw 'Restore-PusulaBackup.ps1 is supported only on Windows.'
}

if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
    throw 'LOCALAPPDATA is not available for the current Windows account.'
}

if ([string]::IsNullOrWhiteSpace($TargetDatabasePath)) {
    $TargetDatabasePath = Join-Path $env:LOCALAPPDATA 'com.stronganchor.pusula\data\pusula.sqlite3'
}
if ([string]::IsNullOrWhiteSpace($StagingRoot)) {
    $StagingRoot = Join-Path $env:LOCALAPPDATA 'com.stronganchor.pusula\restore-staging'
}
if ([string]::IsNullOrWhiteSpace($RollbackRoot)) {
    $RollbackRoot = Join-Path $env:LOCALAPPDATA 'com.stronganchor.pusula\restore-rollbacks'
}

$ciphertext = Resolve-ExistingFile -Path $CiphertextPath -Description 'Encrypted backup'
$recoveryIdentity = Resolve-ExistingFile -Path $RecoveryIdentityPath -Description 'Recovery identity'
$targetDatabase = Get-FullPath -Path $TargetDatabasePath
$stagingRootPath = Get-FullPath -Path $StagingRoot
$rollbackRootPath = Get-FullPath -Path $RollbackRoot
$rageExecutable = Resolve-Executable -Path $RagePath -Description 'rage executable'
$pythonExecutable = Resolve-Executable -Path $PythonPath -Description 'Python 3 executable'

Assert-OutsideSyncRoots -Path $targetDatabase -Description 'Target database'
Assert-OutsideSyncRoots -Path $stagingRootPath -Description 'Staging root'
Assert-OutsideSyncRoots -Path $rollbackRootPath -Description 'Rollback root'
Assert-OutsideSyncRoots -Path $recoveryIdentity -Description 'Recovery identity'
Assert-NoReparsePoints -Path $targetDatabase -Description 'Target database'
Assert-NoReparsePoints -Path $stagingRootPath -Description 'Staging root'
Assert-NoReparsePoints -Path $rollbackRootPath -Description 'Rollback root'
Assert-NoReparsePoints -Path $recoveryIdentity -Description 'Recovery identity'
if ($Apply) {
    Assert-SameVolume -FirstPath $targetDatabase -SecondPath $rollbackRootPath
}

if ([string]::Equals($ciphertext, $targetDatabase, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw 'Encrypted backup and target database paths must be different.'
}
if ([string]::Equals($recoveryIdentity, $targetDatabase, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw 'Recovery identity and target database paths must be different.'
}

$expectedEvidence = $null
if (-not [string]::IsNullOrWhiteSpace($ExpectedEvidencePath)) {
    $expectedEvidence = Read-ExpectedEvidence -Path $ExpectedEvidencePath
}
if ($Apply -and $null -eq $expectedEvidence) {
    throw '-Apply requires -ExpectedEvidencePath with independently recorded metadata, counts, and totals.'
}

Assert-PusulaStopped

$ciphertextItem = Get-Item -LiteralPath $ciphertext
$ciphertextSize = [long]$ciphertextItem.Length
$ciphertextSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $ciphertext).Hash.ToLowerInvariant()

$incidentId = '{0}-{1}' -f (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ'), ([guid]::NewGuid().ToString('N'))
$script:StagingDirectory = Join-Path $stagingRootPath $incidentId
$helperPath = Join-Path $script:StagingDirectory 'pusula-restore-sqlite-helper.py'
$stagedDatabase = Join-Path $script:StagingDirectory 'pusula-restored.sqlite3'

try {
    New-PrivateDirectory -Path $script:StagingDirectory
    Assert-NoReparsePoints -Path $script:StagingDirectory -Description 'Staging directory'
    [System.IO.File]::WriteAllText(
        $helperPath,
        $script:SqliteHelper,
        (New-Object System.Text.UTF8Encoding($false))
    )

    Invoke-External `
        -Executable $rageExecutable `
        -Arguments @('--decrypt', '--identity', $recoveryIdentity, '--output', $stagedDatabase, $ciphertext) `
        -FailureMessage 'rage could not decrypt the Pusula backup'

    if (-not [System.IO.File]::Exists($stagedDatabase)) {
        throw 'rage reported success but did not create a restored database.'
    }
    Write-FileDurably -Path $stagedDatabase

    $databaseResult = Test-PusulaDatabase `
        -Path $stagedDatabase `
        -PythonExecutable $pythonExecutable `
        -HelperPath $helperPath

    if ($null -ne $expectedEvidence) {
        Assert-EvidenceMatches `
            -Evidence $expectedEvidence `
            -DatabaseResult $databaseResult `
            -CiphertextSha256 $ciphertextSha256 `
            -CiphertextSize $ciphertextSize
    }

    $databaseSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $stagedDatabase).Hash.ToLowerInvariant()
    $report = [pscustomobject][ordered]@{
        operation = 'validated'
        schema_version = [long]$databaseResult.schema_version
        integrity_check = [string]$databaseResult.integrity_check
        foreign_key_violations = [long]$databaseResult.foreign_key_violations
        counts = $databaseResult.counts
        totals = $databaseResult.totals
        ciphertext_sha256 = $ciphertextSha256
        ciphertext_size_bytes = $ciphertextSize
        database_sha256 = $databaseSha256
        target_database_path = $targetDatabase
        rollback_directory = $null
    }

    if (-not $Apply) {
        $report | ConvertTo-Json -Depth 6
        return
    }

    if (-not $PSCmdlet.ShouldProcess(
        $targetDatabase,
        'replace the Pusula production database with the validated encrypted backup'
    )) {
        $report.operation = 'validated_what_if'
        $report | ConvertTo-Json -Depth 6
        return
    }

    $targetDirectory = [System.IO.Path]::GetDirectoryName($targetDatabase)
    [System.IO.Directory]::CreateDirectory($targetDirectory) | Out-Null
    Assert-NoReparsePoints -Path $targetDatabase -Description 'Target database'
    $script:LockPath = Join-Path $targetDirectory '.pusula-restore.lock'
    Assert-NoReparsePoints -Path $script:LockPath -Description 'Restore lock'
    try {
        $script:LockStream = [System.IO.File]::Open(
            $script:LockPath,
            [System.IO.FileMode]::OpenOrCreate,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None
        )
    }
    catch {
        throw 'Another Pusula restore appears to be running.'
    }

    Assert-PusulaStopped

    $rollbackDirectory = Join-Path $rollbackRootPath $incidentId
    $script:RollbackDirectory = $rollbackDirectory
    New-PrivateDirectory -Path $rollbackDirectory
    Assert-NoReparsePoints -Path $rollbackDirectory -Description 'Rollback directory'
    $targetExisted = [System.IO.File]::Exists($targetDatabase)
    $rollbackDatabase = Join-Path $rollbackDirectory 'pusula-before-restore.sqlite3'
    $rollbackResult = $null
    $originalBaseSha256 = $null

    if ($targetExisted) {
        Invoke-SqliteHelper `
            -PythonExecutable $pythonExecutable `
            -HelperPath $helperPath `
            -Arguments @('backup', $targetDatabase, $rollbackDatabase) `
            -FailureMessage 'Could not create a consistent SQLite rollback'
        Write-FileDurably -Path $rollbackDatabase
        $rollbackResult = Test-PusulaDatabase `
            -Path $rollbackDatabase `
            -PythonExecutable $pythonExecutable `
            -HelperPath $helperPath

        [System.IO.File]::Copy(
            $targetDatabase,
            (Join-Path $rollbackDirectory 'raw-pusula.sqlite3'),
            $false
        )
        $originalBaseSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $targetDatabase).Hash.ToLowerInvariant()
        $rollbackEvidence = [pscustomobject][ordered]@{
            schema_version = [long]$rollbackResult.schema_version
            integrity_check = [string]$rollbackResult.integrity_check
            foreign_key_violations = [long]$rollbackResult.foreign_key_violations
            counts = $rollbackResult.counts
            totals = $rollbackResult.totals
            database_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $rollbackDatabase).Hash.ToLowerInvariant()
            created_at = (Get-Date).ToUniversalTime().ToString('o')
        }
        [System.IO.File]::WriteAllText(
            (Join-Path $rollbackDirectory 'rollback-evidence.json'),
            ($rollbackEvidence | ConvertTo-Json -Depth 6),
            (New-Object System.Text.UTF8Encoding($false))
        )
    }

    $script:CandidatePath = Join-Path $targetDirectory ('.pusula-restore-{0}.sqlite3' -f [guid]::NewGuid().ToString('N'))
    [System.IO.File]::Copy($stagedDatabase, $script:CandidatePath, $false)
    Write-FileDurably -Path $script:CandidatePath
    $candidateResult = Test-PusulaDatabase `
        -Path $script:CandidatePath `
        -PythonExecutable $pythonExecutable `
        -HelperPath $helperPath
    if (-not (Test-DatabaseResultEqual -Left $databaseResult -Right $candidateResult)) {
        throw 'Copied restore candidate does not match the validated staged database.'
    }
    $candidateSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $script:CandidatePath).Hash.ToLowerInvariant()
    if (-not [string]::Equals($candidateSha256, $databaseSha256, [System.StringComparison]::Ordinal)) {
        throw 'Copied restore candidate SHA-256 does not match the staged database.'
    }

    Assert-PusulaStopped
    foreach ($path in @($targetDatabase, "$targetDatabase-wal", "$targetDatabase-shm")) {
        Assert-ExclusiveFileAccess -Path $path
    }

    $detachedSidecars = New-Object System.Collections.Generic.List[object]
    $replacementCompleted = $false
    $swapReportedErrorAfterChange = $false
    try {
        if ($targetExisted) {
            $script:KeepRollback = $true
        }
        foreach ($suffix in @('-wal', '-shm')) {
            $sourceSidecar = "$targetDatabase$suffix"
            if ([System.IO.File]::Exists($sourceSidecar)) {
                $destinationSidecar = Join-Path $rollbackDirectory ("detached-pusula.sqlite3$suffix")
                [System.IO.File]::Move($sourceSidecar, $destinationSidecar)
                $detachedSidecars.Add([pscustomobject]@{
                    Source = $sourceSidecar
                    Destination = $destinationSidecar
                })
            }
        }

        Assert-PusulaStopped
        if ($targetExisted) {
            $replacedBasePath = Join-Path $rollbackDirectory 'replaced-base-pusula.sqlite3'
            [System.IO.File]::Replace(
                $script:CandidatePath,
                $targetDatabase,
                $replacedBasePath,
                $true
            )
        }
        else {
            [System.IO.File]::Move($script:CandidatePath, $targetDatabase)
        }
        $replacementCompleted = $true
        $script:KeepRollback = $true
        $script:CandidatePath = $null
    }
    catch {
        $replaceError = $_
        $targetShaAfterError = $null
        try {
            if ([System.IO.File]::Exists($targetDatabase)) {
                $targetShaAfterError = (Get-FileHash -Algorithm SHA256 -LiteralPath $targetDatabase).Hash.ToLowerInvariant()
            }
        }
        catch {
            $targetShaAfterError = $null
        }

        if (-not [string]::IsNullOrWhiteSpace($targetShaAfterError) -and
            [string]::Equals($targetShaAfterError, $databaseSha256, [System.StringComparison]::Ordinal)) {
            # ReplaceFile can report a late error after changing file state. Do
            # not reunite an old WAL with the new base; post-validation below
            # deliberately triggers the verified automatic rollback instead.
            $replacementCompleted = $true
            $swapReportedErrorAfterChange = $true
            $script:KeepRollback = $true
            if (-not [System.IO.File]::Exists($script:CandidatePath)) {
                $script:CandidatePath = $null
            }
        }
        elseif ($targetExisted -and
            -not [string]::IsNullOrWhiteSpace($targetShaAfterError) -and
            [string]::Equals($targetShaAfterError, $originalBaseSha256, [System.StringComparison]::Ordinal)) {
            foreach ($sidecar in $detachedSidecars) {
                if ([System.IO.File]::Exists($sidecar.Destination) -and -not [System.IO.File]::Exists($sidecar.Source)) {
                    [System.IO.File]::Move($sidecar.Destination, $sidecar.Source)
                }
            }
            $script:KeepRollback = $false
            throw $replaceError
        }
        elseif (-not $targetExisted -and -not [System.IO.File]::Exists($targetDatabase)) {
            $script:KeepRollback = $false
            throw $replaceError
        }
        else {
            $script:KeepRollback = $true
            throw "CRITICAL: Windows replacement returned an error and target state is ambiguous. Keep Pusula closed and recover from $rollbackDirectory."
        }
    }

    try {
        $postRestoreResult = Test-PusulaDatabase `
            -Path $targetDatabase `
            -PythonExecutable $pythonExecutable `
            -HelperPath $helperPath
        $postRestoreSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $targetDatabase).Hash.ToLowerInvariant()
        if (-not (Test-DatabaseResultEqual -Left $databaseResult -Right $postRestoreResult) -or
            -not [string]::Equals($postRestoreSha256, $databaseSha256, [System.StringComparison]::Ordinal)) {
            throw 'Reopened production database does not match the validated restore candidate.'
        }
        if ($swapReportedErrorAfterChange) {
            throw 'Windows reported an error after changing the target; automatic rollback is required.'
        }
    }
    catch {
        $postValidationFailure = $_
        $failedRestore = Join-Path $rollbackDirectory 'failed-restored-pusula.sqlite3'
        if ([System.IO.File]::Exists($targetDatabase)) {
            [System.IO.File]::Copy($targetDatabase, $failedRestore, $true)
        }

        if ($targetExisted) {
            $rollbackCandidate = Join-Path $targetDirectory ('.pusula-rollback-{0}.sqlite3' -f [guid]::NewGuid().ToString('N'))
            [System.IO.File]::Copy($rollbackDatabase, $rollbackCandidate, $false)
            Write-FileDurably -Path $rollbackCandidate
            foreach ($suffix in @('-wal', '-shm')) {
                Remove-PlaintextPath -Path "$targetDatabase$suffix"
            }
            [System.IO.File]::Replace(
                $rollbackCandidate,
                $targetDatabase,
                (Join-Path $rollbackDirectory 'failed-replacement-base.sqlite3'),
                $true
            )
            $restoredRollbackResult = Test-PusulaDatabase `
                -Path $targetDatabase `
                -PythonExecutable $pythonExecutable `
                -HelperPath $helperPath
            if (-not (Test-DatabaseResultEqual -Left $rollbackResult -Right $restoredRollbackResult)) {
                throw 'Post-restore validation failed and automatic rollback verification also failed.'
            }
        }
        else {
            Remove-PlaintextPath -Path $targetDatabase
        }
        throw "Post-restore validation failed; the previous database was restored. $($postValidationFailure.Exception.Message)"
    }

    $report.operation = 'restored'
    $report.rollback_directory = if ($targetExisted) { $rollbackDirectory } else { $null }
    if (-not $targetExisted) {
        $script:KeepRollback = $false
        Remove-PlaintextPath -Path $rollbackDirectory
        $script:RollbackDirectory = $null
    }
    $report | ConvertTo-Json -Depth 6
}
finally {
    if ($null -ne $script:LockStream) {
        $script:LockStream.Dispose()
        $script:LockStream = $null
    }

    $cleanupFailures = New-Object System.Collections.Generic.List[string]
    foreach ($path in @(
        $script:CandidatePath,
        $script:StagingDirectory,
        $(if (-not $script:KeepRollback) { $script:RollbackDirectory } else { $null })
    )) {
        try {
            Remove-PlaintextPath -Path $path
        }
        catch {
            if (-not [string]::IsNullOrWhiteSpace($path)) {
                $cleanupFailures.Add($path)
            }
        }
    }

    try {
        if (-not [string]::IsNullOrWhiteSpace($script:LockPath) -and [System.IO.File]::Exists($script:LockPath)) {
            [System.IO.File]::Delete($script:LockPath)
        }
    }
    catch {
        $cleanupFailures.Add($script:LockPath)
    }

    if ($cleanupFailures.Count -gt 0) {
        throw "Restore operation ended, but cleanup failed for: $($cleanupFailures -join ', '). Restrict access and remove these paths immediately."
    }
}
