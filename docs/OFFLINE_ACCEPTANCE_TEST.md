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

## Backup, restore, and update

- [ ] An offline change produces a consistent encrypted local backup.
- [ ] Enrollment stores no token in app files/logs and the one-time code cannot
  be replayed.
- [ ] Reconnect the network; the desktop uploads ciphertext directly through a
  short-lived single-object URL and reports server confirmation.
- [ ] The gateway and B2 metadata agree on ciphertext size and SHA-256.
- [ ] Restore that object into a clean test profile using the recovery runbook;
  SQLite integrity, row counts, and financial totals match the source.
- [ ] Install the previous signed version, make an offline write, reconnect, and
  update in-app to the candidate version.
- [ ] The updater rejects a package with an invalid Tauri signature.
- [ ] The pre-update backup completes before installation and all records remain
  intact after relaunch.

## Release evidence

Attach only non-sensitive evidence: installer/update hashes, signature status,
test versions, fixture manifest values, gateway backup ID, B2 object metadata,
and pass/fail results. Never attach customer exports, tokens, keys, or database
files to GitHub.
