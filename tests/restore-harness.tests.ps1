#Requires -Version 5.1

[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$restoreScript = Join-Path $repoRoot 'scripts\Restore-PusulaBackup.ps1'
$python = (Get-Command python.exe -ErrorAction Stop).Source
$testRoot = Join-Path $env:TEMP ('pusula-restore-tests-' + [guid]::NewGuid().ToString('N'))
$stageRoot = Join-Path $testRoot 'stage'
$rollbackRoot = Join-Path $testRoot 'rollbacks'

function Assert-True {
    param(
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if (-not $Condition) {
        throw "ASSERTION FAILED: $Message"
    }
}

function Assert-Equal {
    param(
        [Parameter(Mandatory = $true)]$Expected,
        [Parameter(Mandatory = $true)]$Actual,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if ($Expected -ne $Actual) {
        throw "ASSERTION FAILED: $Message. Expected '$Expected', got '$Actual'."
    }
}

function Assert-Throws {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Action,
        [Parameter(Mandatory = $true)][string]$Pattern,
        [Parameter(Mandatory = $true)][string]$Message
    )

    try {
        & $Action
    }
    catch {
        if ($_.Exception.Message -notlike "*$Pattern*") {
            throw "ASSERTION FAILED: $Message. Unexpected error: $($_.Exception.Message)"
        }
        return
    }
    throw "ASSERTION FAILED: $Message. Expected an exception containing '$Pattern'."
}

function Assert-ThrowsRedacted {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Action,
        [Parameter(Mandatory = $true)][string]$RequiredPattern,
        [Parameter(Mandatory = $true)][string]$ForbiddenPattern,
        [Parameter(Mandatory = $true)][string]$Message
    )

    try {
        & $Action
    }
    catch {
        $errorMessage = $_.Exception.Message
        if ($errorMessage -notlike "*$RequiredPattern*" -or
            $errorMessage -like "*$ForbiddenPattern*") {
            throw "ASSERTION FAILED: $Message. Unexpected error: $errorMessage"
        }
        return
    }
    throw "ASSERTION FAILED: $Message. Expected a redacted exception."
}

function Invoke-RestoreJson {
    param([Parameter(Mandatory = $true)][hashtable]$Parameters)

    $output = (& $restoreScript @Parameters | Out-String)
    return ($output | ConvertFrom-Json)
}

function Assert-StagingEmpty {
    if (-not (Test-Path -LiteralPath $stageRoot)) {
        return
    }
    $remaining = @(Get-ChildItem -LiteralPath $stageRoot -Force -ErrorAction Stop)
    Assert-Equal 0 $remaining.Count 'plaintext staging directory must be empty'
}

$databaseFactory = @'
import os
import sqlite3
import sys

path, variant, mode = sys.argv[1:4]
connection = sqlite3.connect(path)
if mode == "wal_crash":
    connection.execute("PRAGMA journal_mode = WAL")
    connection.execute("PRAGMA wal_autocheckpoint = 0")

connection.executescript("""
CREATE TABLE business_profile (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    name TEXT NOT NULL DEFAULT '', address TEXT NOT NULL DEFAULT '',
    phone TEXT NOT NULL DEFAULT '', website TEXT NOT NULL DEFAULT '',
    footer_sub TEXT NOT NULL DEFAULT ''
);
CREATE TABLE customers (
    id INTEGER PRIMARY KEY, name TEXT NOT NULL, phone TEXT NOT NULL DEFAULT '',
    address TEXT NOT NULL DEFAULT '', work_address TEXT NOT NULL DEFAULT '',
    notes TEXT NOT NULL DEFAULT '', registration_date TEXT NOT NULL
);
CREATE TABLE contacts (
    id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    name TEXT NOT NULL DEFAULT '', phone TEXT NOT NULL DEFAULT '',
    home_address TEXT NOT NULL DEFAULT '', work_address TEXT NOT NULL DEFAULT ''
);
CREATE TABLE sales (
    id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    date TEXT NOT NULL, total_kurus INTEGER NOT NULL CHECK (total_kurus >= 0),
    description TEXT NOT NULL DEFAULT '', request_key TEXT UNIQUE
);
CREATE TABLE installments (
    id INTEGER PRIMARY KEY, sale_id INTEGER NOT NULL REFERENCES sales(id) ON DELETE CASCADE,
    due_date TEXT, amount_kurus INTEGER NOT NULL CHECK (amount_kurus >= 0), paid_date TEXT
);
CREATE TABLE installment_payments (
    id INTEGER PRIMARY KEY,
    installment_id INTEGER NOT NULL REFERENCES installments(id) ON DELETE CASCADE,
    amount_kurus INTEGER NOT NULL CHECK (amount_kurus > 0),
    payment_date TEXT NOT NULL, created_at TEXT NOT NULL, request_key TEXT
);
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE UNIQUE INDEX payments_request_key_idx
    ON installment_payments(request_key) WHERE request_key IS NOT NULL;
PRAGMA user_version = 2;
""")
connection.execute("INSERT INTO business_profile(id, name) VALUES (1, ?)", (variant,))
database_id = (
    "11111111-1111-1111-1111-111111111111"
    if variant == "old"
    else "22222222-2222-2222-2222-222222222222"
)
connection.execute("INSERT INTO settings(key, value) VALUES ('database_id', ?)", (database_id,))
connection.execute("INSERT INTO settings(key, value) VALUES ('onboarding_complete', 'true')")

if variant == "old":
    customers = [(1, "Old customer")]
    sales = [(1, 1, 10000)]
    installments = [(1, 1, 8000)]
    payments = [(1, 1, 2000)]
else:
    customers = [(10, "New customer A"), (20, "New customer B")]
    sales = [(10, 10, 12000), (20, 20, 23000)]
    installments = [(10, 10, 9000), (20, 20, 17000)]
    payments = [(10, 10, 3000), (20, 20, 5000)]

for customer_id, name in customers:
    connection.execute(
        "INSERT INTO customers(id, name, registration_date) VALUES (?, ?, '2026-07-14')",
        (customer_id, name),
    )
connection.execute(
    "INSERT INTO contacts(id, customer_id, name) VALUES (1, ?, 'Contact')",
    (customers[0][0],),
)
for sale_id, customer_id, total in sales:
    connection.execute(
        "INSERT INTO sales(id, customer_id, date, total_kurus) VALUES (?, ?, '2026-07-14', ?)",
        (sale_id, customer_id, total),
    )
for installment_id, sale_id, amount in installments:
    connection.execute(
        "INSERT INTO installments(id, sale_id, due_date, amount_kurus) VALUES (?, ?, '2026-08-14', ?)",
        (installment_id, sale_id, amount),
    )
for payment_id, installment_id, amount in payments:
    connection.execute(
        "INSERT INTO installment_payments(id, installment_id, amount_kurus, payment_date, created_at) "
        "VALUES (?, ?, ?, '2026-07-14', '2026-07-14T12:00:00Z')",
        (payment_id, installment_id, amount),
    )
connection.commit()

if mode == "no_payment_index":
    connection.execute("DROP INDEX payments_request_key_idx")
    connection.commit()

if mode == "wal_crash":
    os._exit(0)
connection.close()
'@

$databaseCountReader = @'
import sqlite3
import sys

connection = sqlite3.connect(sys.argv[1])
print(connection.execute("SELECT COUNT(*) FROM customers").fetchone()[0])
connection.close()
'@

$fakeRage = @'
using System;
using System.IO;

public static class FakeRage
{
    public static int Main(string[] args)
    {
        if (Environment.GetEnvironmentVariable("PUSULA_TEST_RAGE_FAIL") == "1")
        {
            Console.Error.WriteLine("SENSITIVE-RAGE-DIAGNOSTIC " + string.Join(" ", args));
            return 9;
        }

        string output = null;
        for (int index = 0; index < args.Length; index += 1)
        {
            if (args[index] == "--output" && index + 1 < args.Length)
            {
                output = args[index + 1];
            }
        }
        if (output == null || args.Length == 0)
        {
            return 2;
        }
        File.Copy(args[args.Length - 1], output, true);
        return 0;
    }
}
'@

try {
    [System.IO.Directory]::CreateDirectory($testRoot) | Out-Null
    $factoryPath = Join-Path $testRoot 'create-test-database.py'
    $countReaderPath = Join-Path $testRoot 'read-customer-count.py'
    $fakeRagePath = Join-Path $testRoot 'fake-rage.exe'
    $identityPath = Join-Path $testRoot 'test-recovery.agekey'
    $backupPath = Join-Path $testRoot 'backup.sqlite3.age'
    $targetPath = Join-Path $testRoot 'live\pusula.sqlite3'
    $evidencePath = Join-Path $testRoot 'restore-evidence.json'
    [System.IO.File]::WriteAllText($factoryPath, $databaseFactory, (New-Object System.Text.UTF8Encoding($false)))
    [System.IO.File]::WriteAllText($countReaderPath, $databaseCountReader, (New-Object System.Text.UTF8Encoding($false)))
    Add-Type -TypeDefinition $fakeRage -OutputAssembly $fakeRagePath -OutputType ConsoleApplication
    [System.IO.File]::WriteAllText($identityPath, 'test-only identity', (New-Object System.Text.UTF8Encoding($false)))

    & $python $factoryPath $backupPath 'new' 'delete'
    Assert-Equal 0 $LASTEXITCODE 'test backup database creation should succeed'
    [System.IO.Directory]::CreateDirectory((Split-Path -Parent $targetPath)) | Out-Null
    & $python $factoryPath $targetPath 'old' 'wal_crash'
    Assert-Equal 0 $LASTEXITCODE 'WAL-mode live database creation should succeed'
    Assert-True (Test-Path -LiteralPath "$targetPath-wal") 'test fixture should include an uncheckpointed WAL'

    $common = @{
        CiphertextPath = $backupPath
        RecoveryIdentityPath = $identityPath
        TargetDatabasePath = $targetPath
        StagingRoot = $stageRoot
        RollbackRoot = $rollbackRoot
        RagePath = $fakeRagePath
        PythonPath = $python
    }

    $validation = Invoke-RestoreJson -Parameters $common
    Assert-Equal 'validated' $validation.operation 'default mode should validate only'
    Assert-Equal 2 $validation.schema_version 'schema version should be verified'
    Assert-Equal 2 $validation.counts.customers 'customer count should be reported'
    Assert-Equal 35000 $validation.totals.sales_kurus 'sales total should be reported in integer kurus'
    Assert-Equal 26000 $validation.totals.installments_kurus 'installment total should be reported'
    Assert-Equal 8000 $validation.totals.payments_kurus 'payment total should be reported'
    Assert-StagingEmpty

    [System.IO.File]::WriteAllText(
        $evidencePath,
        ($validation | ConvertTo-Json -Depth 6),
        (New-Object System.Text.UTF8Encoding($false))
    )
    $applyParameters = @{} + $common
    $applyParameters.ExpectedEvidencePath = $evidencePath
    $applyParameters.Apply = $true
    $applyParameters.Confirm = $false

    $databaseLockPath = Join-Path (Split-Path -Parent $targetPath) '.pusula-database.lock'
    $databaseLock = [System.IO.File]::Open(
        $databaseLockPath,
        [System.IO.FileMode]::OpenOrCreate,
        [System.IO.FileAccess]::ReadWrite,
        [System.IO.FileShare]::None
    )
    try {
        Assert-Throws {
            & $restoreScript @applyParameters | Out-Null
        } 'database is in use' 'the shared app/restore database lock must stop replacement'
    }
    finally {
        $databaseLock.Dispose()
    }

    $failureTarget = Join-Path $testRoot 'failure-live\pusula.sqlite3'
    [System.IO.Directory]::CreateDirectory((Split-Path -Parent $failureTarget)) | Out-Null
    & $python $factoryPath $failureTarget 'old' 'wal_crash'
    Assert-Equal 0 $LASTEXITCODE 'failure fixture creation should succeed'
    $failureParameters = @{} + $applyParameters
    $failureParameters.TargetDatabasePath = $failureTarget
    $env:PUSULA_TEST_FAIL_AFTER_SIDECAR_DETACH = '1'
    try {
        Assert-Throws {
            & $restoreScript @failureParameters | Out-Null
        } 'Injected failure after sidecar detach' 'verified rollback should run after a sidecar-detach failure'
    }
    finally {
        Remove-Item Env:\PUSULA_TEST_FAIL_AFTER_SIDECAR_DETACH -ErrorAction SilentlyContinue
    }
    Assert-True (Test-Path -LiteralPath "$failureTarget-wal") 'verified rollback should reunite the original WAL'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path (Split-Path -Parent $failureTarget) '.pusula-restore-in-progress.json'))) 'verified rollback should clear the durable restore marker'
    $failureCount = & $python $countReaderPath $failureTarget
    Assert-Equal 1 ([int]$failureCount) 'verified rollback should preserve WAL-only committed rows'

    $cleanupFailureTarget = Join-Path $testRoot 'cleanup-failure-live\pusula.sqlite3'
    [System.IO.Directory]::CreateDirectory((Split-Path -Parent $cleanupFailureTarget)) | Out-Null
    & $python $factoryPath $cleanupFailureTarget 'old' 'delete'
    Assert-Equal 0 $LASTEXITCODE 'cleanup-failure fixture creation should succeed'
    $cleanupFailureParameters = @{} + $applyParameters
    $cleanupFailureParameters.TargetDatabasePath = $cleanupFailureTarget
    $env:PUSULA_TEST_FAIL_RECORDED_ARTIFACT_CLEANUP = '1'
    try {
        Assert-Throws {
            & $restoreScript @cleanupFailureParameters | Out-Null
        } 'Injected failure before recorded restore artifact cleanup' 'artifact cleanup failure must happen before marker deletion'
    }
    finally {
        Remove-Item Env:\PUSULA_TEST_FAIL_RECORDED_ARTIFACT_CLEANUP -ErrorAction SilentlyContinue
    }
    $cleanupFailureMarkerPath = Join-Path (Split-Path -Parent $cleanupFailureTarget) '.pusula-restore-in-progress.json'
    Assert-True (Test-Path -LiteralPath $cleanupFailureMarkerPath) 'artifact cleanup failure must retain the fail-closed marker'
    $cleanupRecoveryParameters = @{
        RecoverInterruptedRestore = $true
        TargetDatabasePath = $cleanupFailureTarget
        StagingRoot = $stageRoot
        RollbackRoot = $rollbackRoot
        PythonPath = $python
        Confirm = $false
    }
    $cleanupRecovery = Invoke-RestoreJson -Parameters $cleanupRecoveryParameters
    Assert-Equal 'recovered_interrupted_restore' $cleanupRecovery.operation 'cleanup-failure marker should remain explicitly recoverable'
    Assert-Equal 1 ([int](& $python $countReaderPath $cleanupFailureTarget)) 'cleanup-failure recovery should restore the recorded original'
    Assert-True (-not (Test-Path -LiteralPath $cleanupFailureMarkerPath)) 'verified cleanup-failure recovery should clear the marker'

    $interruptedTarget = Join-Path $testRoot 'interrupted-live\pusula.sqlite3'
    [System.IO.Directory]::CreateDirectory((Split-Path -Parent $interruptedTarget)) | Out-Null
    & $python $factoryPath $interruptedTarget 'old' 'wal_crash'
    Assert-Equal 0 $LASTEXITCODE 'interrupted fixture creation should succeed'
    $interruptedParameters = @{} + $applyParameters
    $interruptedParameters.TargetDatabasePath = $interruptedTarget
    $env:PUSULA_TEST_LEAVE_INTERRUPTED_AFTER_SIDECAR_DETACH = '1'
    try {
        Assert-Throws {
            & $restoreScript @interruptedParameters | Out-Null
        } 'Injected interrupted restore' 'the harness should retain an interrupted restore incident'
    }
    finally {
        Remove-Item Env:\PUSULA_TEST_LEAVE_INTERRUPTED_AFTER_SIDECAR_DETACH -ErrorAction SilentlyContinue
    }
    $interruptedMarkerPath = Join-Path (Split-Path -Parent $interruptedTarget) '.pusula-restore-in-progress.json'
    Assert-True (Test-Path -LiteralPath $interruptedMarkerPath) 'an ambiguous restore must retain its durable marker'
    $interruptedMarker = [System.IO.File]::ReadAllText($interruptedMarkerPath) | ConvertFrom-Json
    Assert-True (Test-Path -LiteralPath $interruptedMarker.staging_directory_path) 'a crash simulation should retain its recorded plaintext staging directory'
    Assert-True (Test-Path -LiteralPath $interruptedMarker.candidate_database_path) 'a crash simulation should retain its recorded plaintext candidate'
    Assert-Throws {
        & $restoreScript @interruptedParameters | Out-Null
    } 'Do not delete it' 'a normal rerun must refuse to overwrite an interrupted marker'
    $interruptedRecoveryParameters = @{
        RecoverInterruptedRestore = $true
        TargetDatabasePath = $interruptedTarget
        StagingRoot = $stageRoot
        RollbackRoot = $rollbackRoot
        PythonPath = $python
        Confirm = $false
    }
    $interruptedRecovery = Invoke-RestoreJson -Parameters $interruptedRecoveryParameters
    Assert-Equal 'recovered_interrupted_restore' $interruptedRecovery.operation 'explicit recovery should restore the verified rollback'
    Assert-Equal 1 ([int](& $python $countReaderPath $interruptedTarget)) 'interrupted recovery should restore WAL-only committed rows'
    Assert-True (-not (Test-Path -LiteralPath $interruptedMarkerPath)) 'verified interrupted recovery should clear its marker'
    Assert-True (-not (Test-Path -LiteralPath "$interruptedTarget-wal")) 'interrupted recovery should not leave a live WAL sidecar'
    Assert-True (-not (Test-Path -LiteralPath $interruptedMarker.staging_directory_path)) 'interrupted recovery should remove the recorded plaintext staging directory'
    Assert-True (-not (Test-Path -LiteralPath $interruptedMarker.candidate_database_path)) 'interrupted recovery should move or remove the recorded plaintext candidate'

    $tamperedTarget = Join-Path $testRoot 'tampered-interrupted-live\pusula.sqlite3'
    [System.IO.Directory]::CreateDirectory((Split-Path -Parent $tamperedTarget)) | Out-Null
    & $python $factoryPath $tamperedTarget 'old' 'wal_crash'
    Assert-Equal 0 $LASTEXITCODE 'tampered interrupted fixture creation should succeed'
    $tamperedParameters = @{} + $applyParameters
    $tamperedParameters.TargetDatabasePath = $tamperedTarget
    $tamperedStageRoot = Join-Path $testRoot 'tampered-interrupted-stage'
    $tamperedParameters.StagingRoot = $tamperedStageRoot
    $env:PUSULA_TEST_LEAVE_INTERRUPTED_AFTER_SIDECAR_DETACH = '1'
    try {
        Assert-Throws {
            & $restoreScript @tamperedParameters | Out-Null
        } 'Injected interrupted restore' 'tamper fixture should retain an interrupted restore incident'
    }
    finally {
        Remove-Item Env:\PUSULA_TEST_LEAVE_INTERRUPTED_AFTER_SIDECAR_DETACH -ErrorAction SilentlyContinue
    }
    $tamperedMarkerPath = Join-Path (Split-Path -Parent $tamperedTarget) '.pusula-restore-in-progress.json'
    $tamperedMarker = [System.IO.File]::ReadAllText($tamperedMarkerPath) | ConvertFrom-Json
    @'
import sqlite3
import sys
connection = sqlite3.connect(sys.argv[1])
connection.execute("UPDATE customers SET name = 'tampered without changing aggregates' WHERE id = 1")
connection.commit()
connection.close()
'@ | & $python - $tamperedMarker.rollback_database_path
    Assert-Equal 0 $LASTEXITCODE 'rollback tamper fixture should succeed'
    $tamperedRecoveryParameters = @{} + $interruptedRecoveryParameters
    $tamperedRecoveryParameters.TargetDatabasePath = $tamperedTarget
    $tamperedRecoveryParameters.StagingRoot = $tamperedStageRoot
    Assert-Throws {
        & $restoreScript @tamperedRecoveryParameters | Out-Null
    } 'does not match its recorded evidence' 'same-aggregate rollback tampering must fail the SHA-256 gate'
    Assert-True (Test-Path -LiteralPath $tamperedMarkerPath) 'failed tamper recovery must retain its marker'

    $freshInterruptedTarget = Join-Path $testRoot 'fresh-interrupted\pusula.sqlite3'
    $freshInterruptedRollbackRoot = Join-Path $testRoot 'fresh-interrupted-rollbacks'
    $freshInterruptedParameters = @{} + $applyParameters
    $freshInterruptedParameters.TargetDatabasePath = $freshInterruptedTarget
    $freshInterruptedParameters.RollbackRoot = $freshInterruptedRollbackRoot
    $env:PUSULA_TEST_LEAVE_INTERRUPTED_AFTER_SIDECAR_DETACH = '1'
    try {
        Assert-Throws {
            & $restoreScript @freshInterruptedParameters | Out-Null
        } 'Injected interrupted restore' 'clean-machine interruption should retain its marker'
    }
    finally {
        Remove-Item Env:\PUSULA_TEST_LEAVE_INTERRUPTED_AFTER_SIDECAR_DETACH -ErrorAction SilentlyContinue
    }
    [System.IO.File]::Copy($backupPath, $freshInterruptedTarget, $false)
    $freshInterruptedRecoveryParameters = @{} + $interruptedRecoveryParameters
    $freshInterruptedRecoveryParameters.TargetDatabasePath = $freshInterruptedTarget
    $freshInterruptedRecoveryParameters.RollbackRoot = $freshInterruptedRollbackRoot
    $freshInterruptedRecovery = Invoke-RestoreJson -Parameters $freshInterruptedRecoveryParameters
    Assert-Equal 'recovered_interrupted_restore' $freshInterruptedRecovery.operation 'clean-machine recovery should complete explicitly'
    Assert-True (-not (Test-Path -LiteralPath $freshInterruptedTarget)) 'clean-machine recovery should verify removal of a partial target'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path (Split-Path -Parent $freshInterruptedTarget) '.pusula-restore-in-progress.json'))) 'clean-machine recovery should clear its verified marker'

    $malformedTarget = Join-Path $testRoot 'malformed-marker\pusula.sqlite3'
    [System.IO.Directory]::CreateDirectory((Split-Path -Parent $malformedTarget)) | Out-Null
    [System.IO.File]::WriteAllText(
        (Join-Path (Split-Path -Parent $malformedTarget) '.pusula-restore-in-progress.json'),
        '{',
        (New-Object System.Text.UTF8Encoding($false))
    )
    $malformedRecoveryParameters = @{} + $interruptedRecoveryParameters
    $malformedRecoveryParameters.TargetDatabasePath = $malformedTarget
    Assert-Throws {
        & $restoreScript @malformedRecoveryParameters | Out-Null
    } 'not valid JSON' 'malformed interrupted markers must fail closed'

    $outOfRootTarget = Join-Path $testRoot 'out-of-root-marker\pusula.sqlite3'
    $outOfRootDirectory = Split-Path -Parent $outOfRootTarget
    [System.IO.Directory]::CreateDirectory($outOfRootDirectory) | Out-Null
    $outsideRollbackDirectory = Join-Path $testRoot 'untrusted-rollback-location'
    [System.IO.Directory]::CreateDirectory($outsideRollbackDirectory) | Out-Null
    $outOfRootIncident = '20260715T120000Z-00000000000000000000000000000001'
    $outOfRootMarker = [pscustomobject][ordered]@{
        format_version = 3
        incident_id = $outOfRootIncident
        phase = 'database_swap'
        target_database_path = $outOfRootTarget
        target_existed = $false
        rollback_directory = $outsideRollbackDirectory
        rollback_database_path = $null
        rollback_evidence_path = $null
        staging_directory_path = (Join-Path $stageRoot $outOfRootIncident)
        candidate_database_path = (Join-Path $outOfRootDirectory '.pusula-restore-00000000000000000000000000000001.sqlite3')
        created_at = '2026-07-15T12:00:00.0000000Z'
    }
    [System.IO.File]::WriteAllText(
        (Join-Path $outOfRootDirectory '.pusula-restore-in-progress.json'),
        ($outOfRootMarker | ConvertTo-Json -Depth 4 -Compress),
        (New-Object System.Text.UTF8Encoding($false))
    )
    $outOfRootRecoveryParameters = @{} + $interruptedRecoveryParameters
    $outOfRootRecoveryParameters.TargetDatabasePath = $outOfRootTarget
    Assert-Throws {
        & $restoreScript @outOfRootRecoveryParameters | Out-Null
    } 'outside the configured rollback root' 'out-of-root interrupted markers must fail closed'

    $artifactTarget = Join-Path $testRoot 'out-of-root-artifact-marker\pusula.sqlite3'
    $artifactTargetDirectory = Split-Path -Parent $artifactTarget
    [System.IO.Directory]::CreateDirectory($artifactTargetDirectory) | Out-Null
    $artifactIncident = '20260715T120000Z-00000000000000000000000000000002'
    $artifactRollbackDirectory = Join-Path $rollbackRoot $artifactIncident
    [System.IO.Directory]::CreateDirectory($artifactRollbackDirectory) | Out-Null
    $artifactMarker = [pscustomobject][ordered]@{
        format_version = 3
        incident_id = $artifactIncident
        phase = 'database_swap'
        target_database_path = $artifactTarget
        target_existed = $false
        rollback_directory = $artifactRollbackDirectory
        rollback_database_path = $null
        rollback_evidence_path = $null
        staging_directory_path = (Join-Path $testRoot 'outside-staging-root')
        candidate_database_path = (Join-Path $testRoot '.pusula-restore-00000000000000000000000000000002.sqlite3')
        created_at = '2026-07-15T12:00:00.0000000Z'
    }
    [System.IO.File]::WriteAllText(
        (Join-Path $artifactTargetDirectory '.pusula-restore-in-progress.json'),
        ($artifactMarker | ConvertTo-Json -Depth 4 -Compress),
        (New-Object System.Text.UTF8Encoding($false))
    )
    $artifactRecoveryParameters = @{} + $interruptedRecoveryParameters
    $artifactRecoveryParameters.TargetDatabasePath = $artifactTarget
    Assert-Throws {
        & $restoreScript @artifactRecoveryParameters | Out-Null
    } 'outside the configured staging root' 'out-of-root plaintext artifact paths must fail closed'
    Assert-StagingEmpty

    $restored = Invoke-RestoreJson -Parameters $applyParameters
    Assert-Equal 'restored' $restored.operation 'apply mode should report successful replacement'
    Assert-True (-not [string]::IsNullOrWhiteSpace($restored.rollback_directory)) 'existing database should have a rollback directory'
    Assert-True (Test-Path -LiteralPath (Join-Path $restored.rollback_directory 'pusula-before-restore.sqlite3')) 'consistent rollback should be retained'
    Assert-True (Test-Path -LiteralPath (Join-Path $restored.rollback_directory 'raw-pusula.sqlite3')) 'raw pre-restore database should be retained'
    Assert-True (Test-Path -LiteralPath (Join-Path $restored.rollback_directory 'rollback-evidence.json')) 'rollback evidence should be retained'
    Assert-True (Test-Path -LiteralPath (Join-Path $restored.rollback_directory 'detached-pusula.sqlite3-wal')) 'old WAL should be isolated in the rollback directory'
    Assert-True (Test-Path -LiteralPath (Join-Path $restored.rollback_directory 'detached-pusula.sqlite3-shm')) 'old SHM should be isolated in the rollback directory'
    Assert-True (-not (Test-Path -LiteralPath "$targetPath-wal")) 'old WAL must not remain next to restored database'
    Assert-True (-not (Test-Path -LiteralPath "$targetPath-shm")) 'old SHM must not remain next to restored database'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path (Split-Path -Parent $targetPath) '.pusula-restore-in-progress.json'))) 'successful restore should clear the durable restore marker'
    Assert-StagingEmpty

    $rollbackValidationParameters = @{} + $common
    $rollbackValidationParameters.CiphertextPath = Join-Path $restored.rollback_directory 'pusula-before-restore.sqlite3'
    $rollbackValidation = Invoke-RestoreJson -Parameters $rollbackValidationParameters
    Assert-Equal 1 $rollbackValidation.counts.customers 'consistent rollback should include old customer rows from WAL'
    Assert-Equal 10000 $rollbackValidation.totals.sales_kurus 'consistent rollback should include old financial totals from WAL'
    Assert-StagingEmpty

    $freshTarget = Join-Path $testRoot 'fresh\pusula.sqlite3'
    $freshRollbackRoot = Join-Path $testRoot 'fresh-rollbacks'
    $freshParameters = @{} + $common
    $freshParameters.TargetDatabasePath = $freshTarget
    $freshParameters.RollbackRoot = $freshRollbackRoot
    $freshParameters.ExpectedEvidencePath = $evidencePath
    $freshParameters.Apply = $true
    $freshParameters.Confirm = $false
    $freshRestore = Invoke-RestoreJson -Parameters $freshParameters
    Assert-Equal 'restored' $freshRestore.operation 'restore should support a missing target database'
    Assert-True ($null -eq $freshRestore.rollback_directory) 'fresh restore should not report a nonexistent rollback'
    Assert-True (Test-Path -LiteralPath $freshTarget) 'fresh restore should atomically install the database'
    if (Test-Path -LiteralPath $freshRollbackRoot) {
        Assert-Equal 0 @(Get-ChildItem -LiteralPath $freshRollbackRoot -Force).Count 'fresh restore should not retain an unnecessary rollback directory'
    }
    Assert-StagingEmpty

    $beforeMismatchHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $targetPath).Hash
    $mismatchedEvidencePath = Join-Path $testRoot 'mismatched-evidence.json'
    $mismatchedEvidence = ($validation | ConvertTo-Json -Depth 6) | ConvertFrom-Json
    $mismatchedEvidence.counts.customers = 999
    [System.IO.File]::WriteAllText(
        $mismatchedEvidencePath,
        ($mismatchedEvidence | ConvertTo-Json -Depth 6),
        (New-Object System.Text.UTF8Encoding($false))
    )
    $mismatchParameters = @{} + $common
    $mismatchParameters.ExpectedEvidencePath = $mismatchedEvidencePath
    $mismatchParameters.Apply = $true
    $mismatchParameters.Confirm = $false
    Assert-Throws { & $restoreScript @mismatchParameters | Out-Null } 'does not match expected evidence' 'evidence mismatch must stop replacement'
    Assert-Equal $beforeMismatchHash (Get-FileHash -Algorithm SHA256 -LiteralPath $targetPath).Hash 'evidence failure must not modify live database'
    Assert-StagingEmpty

    $corruptBackup = Join-Path $testRoot 'corrupt.sqlite3.age'
    [System.IO.File]::WriteAllText($corruptBackup, 'not sqlite', (New-Object System.Text.UTF8Encoding($false)))
    $corruptParameters = @{} + $common
    $corruptParameters.CiphertextPath = $corruptBackup
    Assert-Throws { & $restoreScript @corruptParameters | Out-Null } 'too short' 'invalid decrypted database must be rejected'
    Assert-StagingEmpty

    $missingIndexBackup = Join-Path $testRoot 'missing-payment-index.sqlite3.age'
    & $python $factoryPath $missingIndexBackup 'new' 'no_payment_index'
    Assert-Equal 0 $LASTEXITCODE 'missing-index fixture creation should succeed'
    $missingIndexParameters = @{} + $common
    $missingIndexParameters.CiphertextPath = $missingIndexBackup
    Assert-Throws {
        & $restoreScript @missingIndexParameters | Out-Null
    } 'database validation failed' 'schema v2 restore must require the payment request-key index'
    Assert-StagingEmpty

    $decryptFailureParameters = @{} + $common
    $env:PUSULA_TEST_RAGE_FAIL = '1'
    try {
        Assert-ThrowsRedacted `
            -Action { & $restoreScript @decryptFailureParameters | Out-Null } `
            -RequiredPattern 'rage could not decrypt' `
            -ForbiddenPattern 'SENSITIVE-RAGE-DIAGNOSTIC' `
            -Message 'native rage diagnostics and identity arguments must be redacted'
    }
    finally {
        Remove-Item Env:\PUSULA_TEST_RAGE_FAIL -ErrorAction SilentlyContinue
    }
    Assert-StagingEmpty

    $fakePusula = Join-Path $testRoot 'pusula-desktop.exe'
    Copy-Item -LiteralPath $env:ComSpec -Destination $fakePusula
    $running = Start-Process -FilePath $fakePusula -ArgumentList '/d', '/c', 'ping -n 30 127.0.0.1 > nul' -PassThru -WindowStyle Hidden
    try {
        Start-Sleep -Milliseconds 300
        Assert-Equal 'pusula-desktop' (Get-Process -Id $running.Id).ProcessName 'test process should exercise the Pusula process-name guard'
        Assert-Throws { & $restoreScript @common | Out-Null } 'Pusula is running' 'running Pusula must stop validation before decryption'
        Assert-StagingEmpty
    }
    finally {
        if (-not $running.HasExited) {
            Stop-Process -Id $running.Id -Force
        }
        $running.WaitForExit(5000) | Out-Null
        $running.Dispose()
    }

    Write-Output 'Restore harness tests passed.'
}
finally {
    if (Test-Path -LiteralPath $testRoot) {
        $cleanupError = $null
        for ($attempt = 0; $attempt -lt 20; $attempt += 1) {
            try {
                Remove-Item -LiteralPath $testRoot -Recurse -Force
                $cleanupError = $null
                break
            }
            catch {
                $cleanupError = $_
                Start-Sleep -Milliseconds 250
            }
        }
        if ($null -ne $cleanupError) {
            throw $cleanupError
        }
    }
}
