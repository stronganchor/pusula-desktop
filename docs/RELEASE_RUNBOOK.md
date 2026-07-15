# Pusula Desktop Release Runbook

Pusula releases are built from one immutable `main` commit. The Windows
installers are Authenticode signed, the lean NSIS installer is also signed
directly with the Tauri updater key embedded in the application, and the exact
candidate assets are published again under a new stable tag without rebuilding
them after acceptance. The detached `.exe.sig` authenticates the same lean
installer bytes used as the updater payload; there is no updater ZIP.

## Repository and environment gates

Before any release:

1. Protect `main`; require a pull request and both `CI / desktop` and
   `CI / gateway` checks.
2. Protect the `windows-release` environment with a required reviewer, prevent
   self-review, and disable administrator bypass where the GitHub plan allows.
   Current live state (2026-07-15) has no required reviewer and administrator
   bypass is enabled, so initial release remains an explicit repository-owner
   gate until GitHub enforcement is tightened.
3. Restrict that environment to `main` and configure all values listed in
   `CODE_SIGNING.md`, including `EXPECTED_WINDOWS_PUBLISHER` and
   `EXPECTED_WINDOWS_CERTIFICATE_SHA256`.
4. Enable immutable GitHub Releases for the repository before creating any
   candidate. Configure the read-only `RELEASE_ADMIN_READ_TOKEN` described in
   `CODE_SIGNING.md`; the signed-release and stable-publication workflows use it
   to check the live repository setting before privileged work. Both workflows
   also require each published release to read back as immutable. The live
   repository setting is enabled.
5. Keep the active tag ruleset named `Protect release tags`. It must target
   tags, include only `refs/tags/v*`, prohibit update and deletion, have no
   bypass actors, and report that the current user can never bypass it. Tag
   creation intentionally remains allowed. The workflows query the ruleset by
   name, not a hard-coded numeric ID.
6. Keep a recoverable administrator copy of the Tauri updater signing key. The
   public key in `src-tauri/tauri.conf.json` must match it; the release workflow
   verifies that relationship before publishing.
7. Before an initial baseline run, generate and retain a one-time random
   password of at least 24 characters, then configure it as the protected
   `windows-release` environment secret
   `ACCEPTANCE_BASELINE_ARCHIVE_PASSWORD`. GitHub cannot reveal a saved secret,
   so the acceptance operator must retain the value in an approved password
   manager until testing is complete.

## Prepare the candidate

1. Bump the same strict SemVer in `package.json`, `package-lock.json`,
   `src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml`, and
   `src-tauri/Cargo.lock`. Keep NSIS downgrades disabled because Pusula's SQLite
   migrations are forward-only; the release-policy test also enforces the
   current-user and offline-WebView installer modes.
2. Run:

   ```powershell
   .\scripts\Test-VersionConsistency.ps1 -ExpectedVersion '0.1.0'
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
   powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tests\release-policy.tests.ps1
   npm run tauri -- build --no-bundle --ci
   npm run tauri -- bundle --bundles nsis --no-sign --ci
   ```

3. Merge through the protected branch and require both CI jobs green. The
   signed-release workflow independently validates the exact gateway source on
   Linux and will not enter its protected signing job unless both its Windows
   desktop and gateway validation jobs pass.
4. From the `main` branch in GitHub Actions, run **Signed Windows Release** with
   the final version. The workflow can publish only a GitHub prerelease
   candidate under the deterministic tag
   `v<version>-candidate.<full-40-character-main-SHA>`; it rejects SemVer
   prerelease suffixes and cannot publish a stable or latest release directly.
   For the initial `0.1.0` release, also set
   `build_initial_acceptance_baseline=true` and
   `acceptance_baseline_version=0.0.9`. Every preflight independently proves
   there is no prior stable release before allowing that flag, requires exactly
   `0.0.9`, and refuses a synthetic baseline after any stable release exists.
   The workflow refuses a moving branch,
   duplicate/non-monotonic version, missing signing value, unexpected
   publisher, invalid updater signature, incomplete asset set, remote asset
   digest mismatch, tag conflict, disabled release immutability, or partial
   publication. It creates the lightweight candidate tag when absent or safely
   resumes that exact protected tag at the same commit. It similarly creates or
   resumes only the exact private prerelease draft, verifies every existing
   numeric asset ID/name/size/digest, and uploads only missing files without
   clobbering. Then one write-token process rechecks the
   live controls, exact numeric release and asset IDs, tag, `main`, and
   case-sensitive remote bytes immediately before publication. It publishes
   these exact candidate assets:

   ```text
   Pusula_<version>_x64_offline-setup.exe
   Pusula_<version>_x64-setup.exe
   Pusula_<version>_x64-setup.exe.sig
   latest.json
   SHA256SUMS.txt
   ```

   `Pusula_<version>_x64-setup.exe` is the Authenticode-signed lean NSIS
   installer and the direct Tauri v2 updater payload. Its adjacent `.exe.sig`
   is the detached Tauri signature referenced by `latest.json`. The separate
   `Pusula_<version>_x64_offline-setup.exe` includes the offline WebView2 runtime
   for disconnected installation and is not downloaded by the in-app updater.

A failure after candidate tag/draft creation is resumable only by rerunning the
failed publication job with the same signed artifact from the same workflow
run. The helper accepts only the exact tag, commit, deterministic private draft,
and matching asset subset, then uploads only missing files. Do not rebuild or
resign over a partial draft, and never delete, retag, or clobber it. A published,
foreign, or mismatched candidate remains an incident and may burn the version;
diagnose it and use a strictly greater version. The optional baseline is uploaded only as the encrypted,
three-day Actions artifact
`pusula-encrypted-initial-acceptance-baseline-<commit>`; it is never attached to
the GitHub Release, and no plaintext baseline is uploaded.

## Exercise the exact prerelease updater

GitHub excludes prereleases from `/releases/latest`, so production installations
do not see a candidate before it is accepted. The candidate's immutable manifest
is nevertheless available at:

```text
https://github.com/stronganchor/pusula-desktop/releases/download/v<version>-candidate.<full-40-character-main-SHA>/latest.json
```

The first release has no prior signed production build, so its private baseline
must be built during the same protected workflow run. The workflow freshly
compiles `0.0.9` before Azure authentication with the embedded updater version
and that exact immutable candidate-tag manifest endpoint. It then signs and
bundles that baseline with the same Tauri public key and Authenticode publisher
as the candidate. It does not rename or rebundle the `0.1.0` executable.

For initial-release acceptance:

1. Download the encrypted baseline Actions artifact from the exact successful
   candidate workflow run. Verify the `.7z` file's SHA-256 against the adjacent
   `.sha256` file before extraction.
2. On the controlled acceptance machine, extract it with 7-Zip and the retained
   one-time password. Do not upload the decrypted installer to a release, send
   it to the customer, or retain it after acceptance. Avoid entering the
   password directly on a command line that will be saved in shell history.

   ```powershell
   $archive = Get-Item 'C:\secure\Pusula_0.0.9_to_0.1.0_acceptance-only.7z'
   $expected = ((Get-Content -Raw "$($archive.FullName).sha256") -split '\s+')[0]
   $actual = (Get-FileHash -Algorithm SHA256 $archive.FullName).Hash.ToLowerInvariant()
   if ($actual -ne $expected) { throw 'Acceptance baseline archive hash mismatch.' }
   ```

   After the hash passes, use the 7-Zip graphical password prompt to extract
   the installer so the password is not retained in PowerShell history.

3. Install it in a clean Windows test profile with no existing Pusula data.
4. Open Pusula, verify the actual installed executable, then open **Veri ve
   Yedekleme** and require the visible value `Pusula sürümü: 0.0.9`. Stop if it
   shows `0.1.0`, `Bilinmiyor`, or any other value.

   ```powershell
   $expectedVersion = '0.0.9'
   $expectedPublisher = '<exact EXPECTED_WINDOWS_PUBLISHER value>'
   $expectedCertificate = '<exact EXPECTED_WINDOWS_CERTIFICATE_SHA256 value>'
   $installedExe = (Get-Process -Name 'pusula-desktop' -ErrorAction Stop |
     Select-Object -First 1).Path
   $productVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo($installedExe).ProductVersion
   if ($productVersion -ne $expectedVersion) { throw "Installed version is $productVersion." }
   $signature = Get-AuthenticodeSignature -LiteralPath $installedExe
   $publisher = $signature.SignerCertificate.GetNameInfo(
     [Security.Cryptography.X509Certificates.X509NameType]::SimpleName,
     $false
   )
   $certificate = ([BitConverter]::ToString(
     [Security.Cryptography.SHA256]::Create().ComputeHash(
       $signature.SignerCertificate.RawData
     )
   )).Replace('-', '').ToLowerInvariant()
   if ($signature.Status -ne 'Valid' -or -not $signature.TimeStamperCertificate -or
       $publisher -cne $expectedPublisher -or $certificate -cne $expectedCertificate) {
     throw "Installed Pusula signature gate failed: $($signature.Status), $publisher, $certificate"
   }
   ```
5. Confirm its updater request resolves only to:

   ```text
   https://github.com/stronganchor/pusula-desktop/releases/download/v0.1.0-candidate.<full-40-character-main-SHA>/latest.json
   ```

6. Complete every item in `OFFLINE_ACCEPTANCE_TEST.md`, including:

   - offline installation and fixture import;
   - single-instance behavior and the disconnected business workflow;
   - updater rejection with a deliberately invalid Tauri signature;
   - candidate download, durable pre-update backup, install, and relaunch;
   - visible version `Pusula sürümü: 0.1.0` after relaunch;
   - encrypted upload and a real recovery-key restore with matching
     counts/totals.
   - repeat the installed-executable signature command with
     `$expectedVersion = '0.1.0'` after the updater relaunches.
   - run the **Controlled invalid-signature drill** with the exact candidate
     `.exe` and `.exe.sig`. Accept only its runtime `result: pass` evidence;
     the harness's preparation-only mode is an automated policy-test fixture,
     not release evidence.
7. Delete the decrypted installer and installed test profile, delete the
   workflow artifact, and remove or rotate
   `ACCEPTANCE_BASELINE_ARCHIVE_PASSWORD` after recording acceptance evidence.

For later releases, exercise the updater from the prior stable signed version.
When a controlled override is needed, generate it outside the repository:

```powershell
.\scripts\New-CandidateUpdaterConfig.ps1 `
  -CandidateVersion '0.2.0' `
  -CandidateTag 'v0.2.0-candidate.<full-40-character-main-SHA>' `
  -OutputPath "$env:TEMP\pusula-0.2.0-candidate.json" `
  -Force
```

Any acceptance-only build with an override must remain private and must be
signed, version-identifiable, and compiled from the intended baseline source.
Never patch, rename, or rebundle a candidate executable to imitate an older
version.

The invalid-signature drill is deliberately separate from that positive update
test. It creates a one-bit-changed copy outside the repository, verifies the
untouched candidate first, and exercises the actual updater download verifier
in a uniquely identified, no-bundle debug app against a loopback-only
manifest. It never changes an immutable release, replaces the production
updater public key, enables an insecure production transport option, installs
the changed executable, or writes to an external service. The generated app,
payload, local manifest, and isolated app data are deleted automatically; keep
and hash only `invalid-signature-evidence.json`.
The harness requires the candidate's full 40-character source commit, verifies
that exact clean `HEAD` before and after the runtime test, and records it in the
pass evidence. Its unique application identifier also gives it a separate
Windows Credential Manager service, so it cannot reuse the production backup
token. Process cleanup and both loopback ports are fail-closed: cleanup failure
prevents pass evidence from being written.

Record only the closed, non-sensitive JSON schema in
`docs/acceptance-evidence.template.json`; do not add notes, operator names,
paths, tokens, URLs, exports, or other free text. Replace every template value
with observed evidence. The fixture binding is the logical manifest checksum
`d709a52df5147bddd57d569d1de4113f76ac10f8841405d970e4e60bdd90ade6`
plus the exact counts and kuruÅŸ totals. It intentionally does not hash the
platform-dependent CRLF/LF representation of the fixture file.

For backup evidence, capture the desktop ciphertext hash/size from the queue
sidecar before confirmed upload removes it. Read the completed gateway record
with the root-only lookup command and independently read the exact B2 object
version. Record the same bounded, non-control version ID for gateway and B2,
the gateway's actual-body verification time in canonical UTC, and identical
desktop/gateway/B2 ciphertext hashes and sizes. Do not record a presigned URL,
device token, runtime key, or recovery identity.

Download the exact five immutable candidate assets into a clean directory,
then produce the only accepted compact UTF-8/no-BOM representation:

```powershell
.\scripts\New-ReleaseAcceptanceEvidence.ps1 `
  -InputPath 'C:\secure\pusula-acceptance-evidence-input.json' `
  -OutputPath 'C:\secure\pusula-acceptance-evidence.json' `
  -CandidateAssetDirectory 'C:\secure\pusula-0.1.0-candidate' `
  -Repository 'stronganchor/pusula-desktop' `
  -Version '0.1.0' `
  -CandidateTag 'v0.1.0-candidate.<full-40-character-main-SHA>' `
  -CandidateCommit '<full-40-character-main-SHA>' `
  -ExpectedWindowsPublisher '<exact protected variable>' `
  -ExpectedWindowsCertificateSha256 '<exact protected variable>'
```

The producer validates the strict closed schema, committed logical fixture,
exact five asset names/sizes/SHA-256 values, both signed identities, all pass
states, backup equality, restore totals, and invalid-signature source binding.
It prints the canonical SHA-256 and strict standard base64 needed for dispatch.
The promotion workflow rejects duplicate/unknown/missing fields, non-canonical
JSON, whitespace or non-canonical base64, more than 60 KiB encoded, or more
than 45 KiB decoded.

## Publish stable without rebuilding

Do not merge another commit while a candidate is under acceptance. After every
acceptance item passes, run **Publish Accepted Windows Release** from the exact
candidate commit on `main` with:

- the exact candidate version;
- its exact immutable `v<version>-candidate.<40-character-SHA>` tag;
- the canonical acceptance evidence SHA-256;
- the exact canonical JSON encoded as strict standard base64;
- confirmation `PROMOTE v<version>`.

The workflow requires immutable releases to be enabled, requires the candidate
itself to report `isImmutable=true`, and proves its tag and release target equal
the current `main` commit. It downloads the candidate, revalidates the exact
asset allowlist, SHA-256 values, Tauri updater signature, Authenticode
publisher, signer-certificate SHA-256, and timestamps. It also binds the
evidence to the successful candidate workflow run ID/repository/commit and
requires the exact fixture counts/totals, clean standard-user offline/restart
drill, positive update, identical desktop/gateway/B2 ciphertext hashes and
sizes, exact matching gateway/B2 version IDs, an in-interval gateway
verification timestamp, empty gateway spool, restored SQLite integrity, and
invalid-signature runtime pass.

The workflow creates the stable lightweight tag `v<version>` at that same
commit and a private draft. A failure after tag or draft creation is resumable:
rerun only with the same version, candidate, commit, evidence digest, and six
local files. The helper accepts only an exact protected tag and exact private
draft, verifies every existing numeric asset ID/name/size/digest, and uploads
only missing assets without `--clobber`. It never deletes or retags. Any
mismatch is a preserved release incident, not something to repair in place.

Stable contains the five candidate files byte-for-byte plus
`Pusula_<version>_acceptance-evidence.json` as the sixth asset. The final helper
uses one contents-write-token process to recheck repository controls, candidate
identity, `main`, both exact refs, numeric release ID, and each numeric asset
ID/name/size/digest immediately before a numeric PATCH. It then verifies the
immutable readback, exact remote evidence digest, GitHub release attestation,
and latest-release identity. It does not rebuild, resign, or edit the immutable
candidate.

GitHub exposes no documented conditional transaction that makes "publish only
if this ref and these asset digests still match" atomic with the release PATCH.
There is therefore a residual read-to-PATCH race even though revalidation and
PATCH are adjacent in one process. During release, grant release/tag write
custody only to these serialized workflows; do not let people or other
automation mutate drafts or `v*` tags. If the post-PATCH immutable readback
fails, preserve the release and logs as an incident. Never delete it to retry.

The copied stable `latest.json` intentionally keeps its updater URL pointed at
the immutable candidate tag. The stable release serves that same manifest
through `/releases/latest`, while the signed updater payload remains anchored to
the exact candidate bytes exercised during acceptance. Keep both immutable
releases permanently.

If acceptance fails, leave the candidate as a prerelease, diagnose it, and
release a strictly greater version. Never replace a published candidate asset.
