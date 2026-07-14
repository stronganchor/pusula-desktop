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
    payment_date TEXT NOT NULL, created_at TEXT NOT NULL
);
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
PRAGMA user_version = 1;
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

if mode == "wal_crash":
    os._exit(0)
connection.close()
'@

$fakeRage = @'
$outputIndex = -1
for ($index = 0; $index -lt $args.Count; $index += 1) {
    if ($args[$index] -eq '--output') {
        $outputIndex = $index + 1
        break
    }
}
if ($outputIndex -lt 0 -or $outputIndex -ge $args.Count) {
    $global:LASTEXITCODE = 2
    throw 'missing --output'
}
Copy-Item -LiteralPath $args[$args.Count - 1] -Destination $args[$outputIndex] -Force
$global:LASTEXITCODE = 0
'@

try {
    [System.IO.Directory]::CreateDirectory($testRoot) | Out-Null
    $factoryPath = Join-Path $testRoot 'create-test-database.py'
    $fakeRagePath = Join-Path $testRoot 'fake-rage.ps1'
    $identityPath = Join-Path $testRoot 'test-recovery.agekey'
    $backupPath = Join-Path $testRoot 'backup.sqlite3.age'
    $targetPath = Join-Path $testRoot 'live\pusula.sqlite3'
    $evidencePath = Join-Path $testRoot 'restore-evidence.json'
    [System.IO.File]::WriteAllText($factoryPath, $databaseFactory, (New-Object System.Text.UTF8Encoding($false)))
    [System.IO.File]::WriteAllText($fakeRagePath, $fakeRage, (New-Object System.Text.UTF8Encoding($false)))
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
    Assert-Equal 1 $validation.schema_version 'schema version should be verified'
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
