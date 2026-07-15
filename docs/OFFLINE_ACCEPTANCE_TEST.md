# Initial Release Offline Acceptance Test

Record the application version, Windows version, tester, date, and the expected
manifest counts/totals. Run this drill in a clean Windows user profile before
each production release.

## Installation and migration

- [ ] Windows shows a valid Strong Anchor Authenticode signature on the offline
  installer.
- [ ] Installation succeeds without internet access or administrator rights.
- [ ] First launch offers import or an explicitly confirmed empty start.
- [ ] The empty-start acknowledgement is bound to that SQLite database; moving
  the database away makes the replacement empty database show first-run again.
- [ ] A second Pusula launch focuses the existing window instead of opening a
  competing writer.
- [ ] The fixture/production handoff imports with the expected SHA-256, counts,
  and financial totals.
- [ ] A deliberately corrupted checksum and an orphaned relationship are both
  rejected without changing existing records.
- [ ] A failed SQLite integrity result blocks the business interface and points
  the operator to the recovery runbook.

## Offline business workflow

Disable Wi-Fi and Ethernet and confirm Windows has no route to the internet.

- [ ] Search by customer ID, name, phone, and address.
- [ ] Add and edit a customer and both contacts; restart and verify persistence.
- [ ] Record a cash sale.
- [ ] Record a sale with a down payment and installments; verify that the sale
  and every installment appear together.
- [ ] Record a partial payment, a final payment, and a same-day reversal.
- [ ] Double-click or otherwise trigger one partial-payment submit twice; only
  one payment row and one financial increment exist. Repeat an exact pending
  submit after restarting the app and confirm the durable request key replays
  the same row; changing amount or date must not reuse that key.
- [ ] Verify daily collections and expected-payment filters and totals.
- [ ] Print a sale receipt and a payment receipt.
- [ ] Restart Windows and verify all records and reports remain available.

## Failure atomicity

- [ ] Terminate the app during a test import; the prior database still passes
  integrity checking and has its original counts/totals.
- [ ] Supply invalid sale/installment input; no partial sale exists afterward.
- [ ] Fill or make the backup destination unwritable; local business writes
  continue and the backup status clearly reports the failure.
- [ ] A destructive import refuses to start when its encrypted pre-import
  snapshot cannot be durably written.
- [ ] Hold the shared database lock from another process; both app startup and
  restore replacement fail closed. Leave a synthetic
  `.pusula-restore-in-progress.json` marker and confirm startup refuses SQLite
  access. Confirm a normal restore refuses to overwrite it, then use
  `-RecoverInterruptedRestore` and require verified rollback/removal plus no
  recorded plaintext staging/candidate remnants before the marker clears.

## Backup, restore, and update

- [ ] An offline change produces a consistent encrypted local backup.
- [ ] Enrollment stores no token in app files/logs and the one-time code cannot
  be replayed.
- [ ] Revoke a device token and require **Yeniden kurulum gerekli** plus visible
  enrollment controls; reenrollment uploads the preserved ciphertext queue.
- [ ] Reconnect the network; the desktop uploads ciphertext directly through a
  short-lived single-object URL and reports server confirmation.
- [ ] On a network where the direct B2 TLS connection fails before any HTTP
  response, the desktop retries the same backup ID through the authenticated
  gateway relay; B2 receives only the existing `.age` ciphertext, the gateway
  spool is empty afterward, and a B2 HTTP rejection does not trigger the relay.
- [ ] The desktop queue sidecar, gateway verification record, and exact B2
  object version agree on ciphertext size and SHA-256. The nonempty gateway and
  B2 version IDs match exactly, and the gateway verification timestamp falls
  within the recorded acceptance interval.
- [ ] Force `uploaded`/`relay_pending` `404 not_found`; Pusula clears only the
  stale binding, re-reserves, and uploads byte-identical ciphertext. Force
  `409 object_not_present`; it preserves the same binding and stage-appropriate
  retry. Force `502 storage_verification_failed`; it confirms later without a
  blind PUT/relay. Lost direct/relay responses must still yield exactly one B2
  object version.
- [ ] Leave the computer off across the first of a month. On the next launch,
  require one daily and one catch-up monthly artifact for the local business
  periods. Re-run the six-hour scheduler in the same periods and require no
  duplicate artifacts; remote retries only drain the existing queue.
- [ ] Restore that object into a clean test profile using the recovery runbook;
  SQLite integrity, row counts, and financial totals match the source.
- [ ] Install the previous signed version, make an offline write, reconnect, and
  update in-app to the candidate version.
- [ ] The controlled invalid-signature harness reports `result: pass`, fetches
  the deliberately changed copy, and rejects it before installation
  confirmation without changing the candidate artifact.
- [ ] The pre-update backup completes before installation and all records remain
  intact after relaunch.
- [ ] Stall a remote backup request, then start update preparation. A local
  business write remains available while preflight waits for its snapshot lane;
  once preflight takes the maintenance gate it creates a durable encrypted
  local snapshot without another gateway request.

### Controlled invalid-signature drill

Use the exact lean candidate executable and adjacent `.exe.sig` downloaded from
the immutable candidate release. Run this from the exact candidate source
commit after `npm ci`, with the same trusted Minisign 0.12 executable used by
the release workflow. The harness requires a new directory outside the
repository and refuses to run its runtime acceptance phase with any Tauri
configuration other than `src-tauri/tauri.conf.json`.

For the initial `0.0.9` to `0.1.0` acceptance test:

```powershell
$assetDirectory = 'C:\secure\pusula-0.1.0-candidate'
$expectedSourceCommit = '<full-40-character-candidate-source-SHA>'
$harnessOutput = Join-Path $env:TEMP `
  ('pusula-invalid-signature-' + [Guid]::NewGuid().ToString('N'))

.\scripts\Test-InvalidTauriUpdaterAcceptance.ps1 `
  -ArtifactPath (Join-Path $assetDirectory 'Pusula_0.1.0_x64-setup.exe') `
  -SignaturePath (Join-Path $assetDirectory 'Pusula_0.1.0_x64-setup.exe.sig') `
  -CandidateVersion '0.1.0' `
  -HarnessVersion '0.0.9' `
  -ExpectedSourceCommit $expectedSourceCommit `
  -MinisignPath 'C:\secure\minisign-0.12\minisign.exe' `
  -OutputDirectory $harnessOutput
```

The harness does not install anything. It first verifies the untouched
candidate and signature, makes a one-bit-changed copy under the new temporary
directory, and proves that copy fails detached-signature verification. It then
builds a no-bundle debug app with a unique application identifier, inheriting
the production updater public key. The only override is a generated loopback
manifest endpoint. Debug mode permits that local HTTP endpoint without adding
any dangerous transport or certificate option to production configuration.
The unique identifier also selects a unique Windows Credential Manager service
named `<isolated-identifier>.backup`, so the harness cannot load the production
backup token or contact the backup gateway as an enrolled device.

The isolated app fetches the local manifest and changed payload. The drill
passes only when the real app updater reports failure during its
download/verification phase and installation confirmation was never called.
The harness stops its exact child processes, removes its isolated app data,
debug build, manifest, and executable copy, and leaves only
`invalid-signature-evidence.json`. Require all of these evidence fields:

```text
result: pass
source_commit: <full-40-character-candidate-source-SHA>
source_clean: true
candidate_unchanged: true
signature_unchanged: true
original_signature_verification: accepted
tampered_copy_signature_verification: rejected
runtime_rejection_phase: downloading
installation_confirmation_called: false
dangerous_updater_overrides: false
production_configuration_modified: false
installer_created_or_run: false
```

Hash the evidence file and record its SHA-256 in the acceptance worksheet. Do
not treat preparation-only test output, a verifier-only failure, or a missing
runtime observation as a pass. This negative-path drill complements, but does
not replace, the signed previous-version-to-candidate update drill.

## Release evidence

Use only the closed schema in `acceptance-evidence.template.json`, then run
`scripts/New-ReleaseAcceptanceEvidence.ps1` as documented in
`RELEASE_RUNBOOK.md`. Stable publication accepts only its canonical compact
UTF-8/no-BOM JSON and attaches those exact bytes as
`Pusula_<version>_acceptance-evidence.json`.

The evidence may contain installer/update hashes, exact signer identity, test
versions, the committed logical fixture-manifest checksum and exact
counts/totals, gateway backup ID, ciphertext size/hash metadata, the exact
matching gateway/B2 object version ID, the gateway verification timestamp, and
enumerated pass results. Never add free-text notes, names, filesystem paths,
customer exports, tokens, keys, credentials, database files, or other sensitive
fields.
