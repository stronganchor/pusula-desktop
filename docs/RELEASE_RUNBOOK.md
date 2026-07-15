# Pusula Desktop release runbook

This runbook publishes a managed single-machine Windows release. The NSIS
installers are intentionally Authenticode `NotSigned`. The initial
browser-downloaded installer requires one explicit SmartScreen acknowledgement;
updates are authenticated by the mandatory Tauri signature embedded in Pusula.

The normal operator can perform the full workflow with the existing GitHub
environment secrets and local DPAPI-protected key custody. Azure Artifact
Signing, a separate reviewer, a Windows certificate, and an administration
token are not prerequisites.

## Non-negotiable gates

Do not publish a stable release until all of these are proven:

- `main` is clean, pushed, and the exact commit under test;
- desktop, frontend, restore, release-policy, and gateway tests pass;
- the full offline and lean updater installers report Authenticode
  `NotSigned`;
- the lean updater's `.exe.sig` verifies against the embedded Tauri key;
- a clean standard-user profile installs the private 0.0.9 baseline offline,
  acknowledges SmartScreen only for that initial installer, and updates in-app
  to the candidate with exactly one Pusula confirmation, zero Windows prompts,
  and no certificate installation;
- the invalid-signature runtime harness rejects a changed updater before
  installation confirmation;
- fixture import, offline business writes, restart persistence, failure
  atomicity, encrypted gateway upload, idempotent retry, independent storage
  readback, and guarded restore all pass; and
- canonical evidence is bound to the exact immutable candidate.

## Existing one-time configuration

The GitHub environment `windows-release` is restricted to `main` and
contains:

- `TAURI_SIGNING_PRIVATE_KEY`
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
- `ACCEPTANCE_BASELINE_ARCHIVE_PASSWORD`

It intentionally has no required reviewer. The packaging job reads these
secrets; publication jobs use only GitHub's job-scoped contents token.

Verify the recoverable public key without printing private material:

```powershell
$config = Get-Content -Raw .\src-tauri\tauri.conf.json | ConvertFrom-Json
$localPublic = (Get-Content -Raw "$env:USERPROFILE\.pusula-secrets\tauri-updater.key.pub").Trim()
if ([string]$config.plugins.updater.pubkey -cne $localPublic) {
  throw 'Recoverable updater public key does not match the application.'
}
(Get-Acl "$env:USERPROFILE\.pusula-secrets\tauri-updater.key").Access
```

The key ACL must allow only the owning Windows account. Never use the
superseded `%USERPROFILE%\.tauri\pusula-desktop.key`.

Release immutability and the no-bypass `v*` update/delete ruleset remain
server controls. They can be inspected with the signed-in owner session:

```powershell
gh api repos/stronganchor/pusula-desktop/immutable-releases
gh api repos/stronganchor/pusula-desktop/rulesets
```

## 1. Prepare the release commit

1. Set the same final SemVer in `package.json`,
   `src-tauri/tauri.conf.json`, and `src-tauri/Cargo.toml`.
2. Run:

   ```powershell
   npm ci
   npm run build
   npm run test:frontend
   cargo fmt --manifest-path src-tauri/Cargo.toml --check
   cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
   cargo test --manifest-path src-tauri/Cargo.toml
   cargo fmt --manifest-path gateway/Cargo.toml --check
   cargo clippy --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
   cargo test --manifest-path gateway/Cargo.toml
   powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tests\restore-harness.tests.ps1
   powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tests\recovery-custody.tests.ps1
   powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tests\release-policy.tests.ps1
   ```

3. Commit and push `main`. Record the full commit and exact green CI run.

The recovery-custody test validates the portable key-kit generator described
in `RECOVERY_KEY_CUSTODY.md`; it uses Git for Windows' bundled `gpg.exe` and
does not expose either production private key.

## 2. Build the immutable candidate

For the first release, dispatch `release.yml` from `main`:

```powershell
gh workflow run release.yml --repo stronganchor/pusula-desktop --ref main -f version=0.1.0 -f build_initial_acceptance_baseline=true -f acceptance_baseline_version=0.0.9
```

The workflow rechecks version/tag/commit identity, runs desktop and gateway
validation, compiles the candidate and private baseline, builds the full
offline installer with `--no-sign`, builds and Tauri-signs the lean updater,
requires Authenticode `NotSigned`, independently verifies the Tauri
signature with pinned Minisign, and publishes an immutable prerelease tagged:

```text
v<version>-candidate.<full-commit>
```

The five candidate assets are:

```text
Pusula_<version>_x64_offline-setup.exe
Pusula_<version>_x64-setup.exe
Pusula_<version>_x64-setup.exe.sig
latest.json
SHA256SUMS.txt
```

The encrypted private baseline is a three-day Actions artifact named
`pusula-encrypted-initial-acceptance-baseline-<candidate-commit>`. It is
never a release asset. Reruns may resume only the same tag, commit, and bytes;
the helpers never delete, clobber, or retag.

## 3. Retrieve the private baseline

Download the Actions artifact, then use the local DPAPI password without
displaying it:

```powershell
$secure = Get-Content "$env:USERPROFILE\.pusula-secrets\acceptance-baseline-password.dpapi" | ConvertTo-SecureString
$pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
try {
  $password = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer)
  & 7z.exe t "-p$password" .\Pusula_0.0.9_to_0.1.0_acceptance-only.7z
  if ($LASTEXITCODE -ne 0) { throw 'Baseline archive verification failed.' }
  & 7z.exe x "-p$password" .\Pusula_0.0.9_to_0.1.0_acceptance-only.7z
  if ($LASTEXITCODE -ne 0) { throw 'Baseline archive extraction failed.' }
} finally {
  if ($pointer -ne [IntPtr]::Zero) {
    [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer)
  }
  Remove-Variable password,secure -ErrorAction SilentlyContinue
}
```

Verify the archive SHA-256 against its checksum and metadata. Keep the extracted
installer outside the repository and delete it after acceptance.

## 4. Run clean-machine acceptance

Use a clean Windows 10/11 x64 standard-user profile. Record exact UTC start and
completion times.

1. Disconnect all network adapters.
2. Verify the baseline installer hash and require
   `Get-AuthenticodeSignature` status `NotSigned`.
3. Run it. After confirming the exact filename, acknowledge **More info** /
   **Run anyway** once. Do not import a certificate.
4. Import `tests/fixtures/pusula-lite-v1.json` and prove exact counts,
   integer-kuruş totals, and manifest checksum.
5. Complete every offline, restart, single-instance, and failure item in
   `OFFLINE_ACCEPTANCE_TEST.md`.
6. Reconnect and use Pusula's in-app updater.
7. Confirm the update once inside Pusula, then require the exact candidate to
   install with zero manual Windows prompts, no certificate installation, and
   all prior records intact.
8. Record baseline and candidate installed-executable SHA-256 values.

A second SmartScreen/installer prompt during the in-app update is a failed
acceptance result. Do not install a self-signed root or disable Windows
security.

## 5. Prove backup and restore

1. Enroll the desktop with the staged gateway.
2. Produce and upload a new encrypted backup.
3. Record backup ID, ciphertext SHA-256/size, gateway SHA-256/size,
   deterministic version ID, and gateway verification time.
4. Use the VPS root-only `download-backup` command for independent readback.
   The desktop queue is not storage evidence.
5. Require:

   ```text
   ciphertext_sha256 == gateway_sha256 == storage_sha256
   desktop_size == gateway_size == storage_size
   gateway_version_id == storage_version_id == fs-sha256-<ciphertext_sha256>
   ```

6. Require an in-interval gateway verification time and empty spool.
7. Retry the same backup ID after a simulated lost response. Require one object
   and the same version ID.
8. Restore the independently read ciphertext with
   `scripts\Restore-PusulaBackup.ps1` and prove SQLite integrity, foreign
   keys, row counts, and financial totals.

The gateway never receives SQLite plaintext or the recovery private key.

## 6. Prove invalid-signature rejection

Run `scripts\Test-InvalidTauriUpdaterAcceptance.ps1` against the exact
candidate `.exe` and `.exe.sig`. It must report `result: pass`,
reject the changed payload during download/verification, and never call
installation confirmation. Record its evidence SHA-256.

## 7. Canonicalize acceptance evidence

Copy `docs\acceptance-evidence.template.json` outside the repository and
replace only placeholders with observed values. Closed schema version 3
requires:

- the exact five candidate assets and workflow run;
- clean standard-user/offline properties;
- `managed_unsigned_single_machine`, `currentUser`, both installer
  statuses `NotSigned`, initial hash/SmartScreen acknowledgements, no trusted
  publisher certificate, verified Tauri signature, exactly one Pusula
  confirmation, and zero Windows/SmartScreen update prompts;
- baseline/candidate executable hashes;
- fixture source/restored counts and totals;
- matching desktop/gateway/storage ciphertext and deterministic
  `fs-sha256-...` version ID; and
- invalid-signature evidence and enumerated pass checks.

Create the canonical file:

```powershell
.\scripts\New-ReleaseAcceptanceEvidence.ps1 -InputPath C:\secure\pusula-acceptance-input.json -OutputPath C:\secure\pusula-acceptance-canonical.json -CandidateAssetDirectory C:\secure\pusula-candidate-assets -Repository stronganchor/pusula-desktop -Version 0.1.0 -CandidateTag 'v0.1.0-candidate.<full-commit>' -CandidateCommit '<full-commit>'
```

Record its SHA-256 and base64. Do not add names, notes, filesystem paths, URLs,
tokens, credentials, customer data, database files, or keys.

## 8. Promote the accepted candidate

Dispatch `promote-release.yml` from the candidate commit with:

```text
version: 0.1.0
candidate_tag: v0.1.0-candidate.<full-commit>
acceptance_evidence_sha256: <64 lowercase hex>
acceptance_evidence_base64: <canonical JSON as strict standard base64>
confirmation: PROMOTE v0.1.0
```

Promotion revalidates the immutable candidate, exact tag/commit/assets,
Tauri signature, both Authenticode `NotSigned` statuses, canonical evidence,
and source workflow run. It adds the evidence as the sixth stable asset and
publishes immutable `v<version>`.

Verify:

```powershell
gh release view v0.1.0 --repo stronganchor/pusula-desktop --json tagName,isDraft,isImmutable,isPrerelease,targetCommitish,assets
gh release verify v0.1.0 --repo stronganchor/pusula-desktop
gh api repos/stronganchor/pusula-desktop/releases/latest --jq .tag_name
```

The stable release must be immutable, non-draft, non-prerelease, target the
accepted commit, contain exactly six assets, and be the latest stable release.

## Later releases

Set `build_initial_acceptance_baseline=false`, update from the previous
stable build, retain the same updater public key, repeat the acceptance drills,
and publish only a strictly greater final SemVer.

If the updater key is lost, recover it or perform a separately managed full
reinstall. Do not rotate the embedded public key and claim existing
installations can update.
