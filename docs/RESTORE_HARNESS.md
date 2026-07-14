# Pusula Windows Restore Harness

`scripts/Restore-PusulaBackup.ps1` is the guarded administrator path for an
encrypted Pusula backup. Validation-only is the default. Replacing the local
production database requires `-Apply`, a high-impact PowerShell confirmation,
and independent evidence for the selected backup.

The harness supports Pusula SQLite schema version 1. A later database migration
must update the harness and its tests before that release is shipped.

## Prerequisites

- Run PowerShell as the same Windows user that runs Pusula. A different account
  has a different `%LOCALAPPDATA%`; if that is unavoidable, pass the exact
  operator database path with `-TargetDatabasePath`.
- Close Pusula. The harness refuses to continue while `Pusula.exe` or
  `pusula-desktop.exe` is running and checks again immediately before the swap.
- Obtain `rage.exe` from the official rage project and install Python 3. Python
  is used only for read-only SQLite validation and SQLite's online backup API.
- Obtain the encrypted `.sqlite3.age` file and the recovery identity through
  the approved custody channels. Do not put the identity in OneDrive, a ticket,
  email, the repository, or the customer computer's normal application data.
- Use a BitLocker-protected local volume. The harness deletes its staging files,
  but normal deletion is not a forensic secure erase, especially on an SSD.

By default the live database, staging root, and rollback root are:

```text
%LOCALAPPDATA%\com.stronganchor.pusula\data\pusula.sqlite3
%LOCALAPPDATA%\com.stronganchor.pusula\restore-staging\
%LOCALAPPDATA%\com.stronganchor.pusula\restore-rollbacks\
```

The target, staging, rollback, and recovery-identity paths must be outside all
configured OneDrive roots. Symbolic links, junctions, and other reparse points
are rejected. The target and rollback root must be on the same Windows volume
so `File.Replace` can perform the live swap atomically.

## Independent evidence

Before a destructive restore, prepare a small JSON evidence file in a protected
local incident directory. Take the encrypted size and SHA-256 from the gateway,
B2 metadata, or the original queue sidecar. Take schema, counts, and totals from
the last recorded Pusula data-status screen, migration/export manifest, or
incident worksheet. Do not derive the "expected" values only from the candidate
being restored; that would confirm internal consistency but not that the
operator selected the right backup.

```json
{
  "schema_version": 1,
  "ciphertext_sha256": "64-lowercase-or-uppercase-hex-characters",
  "ciphertext_size_bytes": 123456,
  "counts": {
    "customers": 100,
    "contacts": 25,
    "sales": 275,
    "installments": 420,
    "payments": 210
  },
  "totals": {
    "sales_kurus": 12500000,
    "installments_kurus": 9000000,
    "payments_kurus": 4500000
  }
}
```

All counts, sizes, and totals are exact nonnegative 64-bit integers. Money is
integer kuruş; do not use decimal lira values.

## Validate without replacing anything

Run validation first. This decrypts into a newly created private staging
directory, checks the SQLite header, `PRAGMA integrity_check`,
`PRAGMA foreign_key_check`, schema version, required tables/columns/settings,
integer-kuruş constraints, counts, and totals, then removes the staging
directory in a `finally` block.

```powershell
Set-Location C:\path\to\pusula-desktop

$candidate = & .\scripts\Restore-PusulaBackup.ps1 `
  -CiphertextPath 'C:\Pusula-Recovery\backup.sqlite3.age' `
  -RecoveryIdentityPath 'E:\Pusula-Keys\pusula-recovery.agekey'

$candidate
```

The JSON report includes ciphertext and plaintext database hashes, schema,
counts, and totals, but no recovery identity, customer fields, tokens, or
credentials. Compare it with the independent evidence. If validation fails,
the target database is untouched and staging is still removed.

Custom tool locations can be supplied when they are not on `PATH`:

```powershell
-RagePath 'C:\AdminTools\rage.exe' -PythonPath 'C:\Python313\python.exe'
```

## Apply the restore

Disconnect the computer from the network, keep Pusula closed, and run:

```powershell
& .\scripts\Restore-PusulaBackup.ps1 `
  -CiphertextPath 'C:\Pusula-Recovery\backup.sqlite3.age' `
  -RecoveryIdentityPath 'E:\Pusula-Keys\pusula-recovery.agekey' `
  -ExpectedEvidencePath 'C:\Pusula-Recovery\expected-evidence.json' `
  -Apply
```

Review the PowerShell high-impact confirmation. `-WhatIf` may be combined with
`-Apply` to exercise decryption and all pre-replacement checks without changing
the target. Reserve `-Confirm:$false` for an already approved and recorded
automated acceptance drill.

For an existing database, the harness:

1. acquires an exclusive restore lock and proves the Pusula files are not open;
2. creates and validates a SQLite-consistent rollback with the online backup
   API, which includes committed data still present in WAL;
3. retains the original base database and moves old `-wal`/`-shm` files into a
   private timestamped rollback directory;
4. copies, flushes, and revalidates the candidate beside the target;
5. replaces the base database atomically with Windows `File.Replace`; and
6. reopens the installed database and requires the same SHA-256, schema, counts,
   totals, integrity result, and foreign-key result.

If the final reopen check fails, the harness preserves the failed candidate,
atomically reinstalls the verified logical rollback, verifies that rollback,
and exits with an error. A clean-machine restore with no prior target does not
create a meaningless rollback.

The retained rollback directory contains plaintext customer data and must stay
access-controlled:

```text
pusula-before-restore.sqlite3       verified logical rollback
rollback-evidence.json             counts, totals, schema, and rollback hash
raw-pusula.sqlite3                 original base file
replaced-base-pusula.sqlite3       base file retained by File.Replace
detached-pusula.sqlite3-wal        present when the old database used WAL
detached-pusula.sqlite3-shm        present when the old database used SHM
```

Do not delete the rollback until the offline acceptance drill and a new
encrypted remote backup both pass. Use the approved data-destruction procedure
when retention ends.

## Post-restore acceptance

1. Keep the network disconnected and start the same or a newer compatible
   Pusula build.
2. Open **VERİ VE YEDEK** and match schema, integrity, all five counts, and all
   three integer-kuruş totals to the restore report.
3. Complete the offline create/edit/payment/report/receipt/restart drill.
4. Reconnect, enroll backup if needed, create a new encrypted backup, and verify
   its remote completion.
5. Record the script report and rollback location in the incident worksheet.
   Never attach the database, recovery identity, or customer data to the ticket.

## Automated test

The test uses temporary synthetic databases and a test-only decrypt shim. It
covers validation-only mode, an atomic clean-machine restore, an existing live
database with committed data only in WAL, rollback reconciliation, evidence
mismatch, corrupt plaintext, process detection, and success/failure cleanup.

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File .\tests\restore-harness.tests.ps1
```

The synthetic test does not replace the required release-gate drill with the
real `rage.exe`, a real app-created encrypted backup, and the signed installed
Pusula build on the designated Windows acceptance profile.
