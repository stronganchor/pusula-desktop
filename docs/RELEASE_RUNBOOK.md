# Pusula Desktop Release Runbook

Pusula releases are built from one immutable `main` commit. The Windows
installer is Authenticode signed, the updater archive is signed with the Tauri
key embedded in the application, and the exact candidate assets are published
again under a new stable tag without rebuilding them after acceptance.

## Repository and environment gates

Before any release:

1. Protect `main`; require a pull request and both `CI / desktop` and
   `CI / gateway` checks.
2. Protect the `windows-release` environment with a required reviewer, prevent
   self-review, and disable administrator bypass where the GitHub plan allows.
3. Restrict that environment to `main` and configure all values listed in
   `CODE_SIGNING.md`, including `EXPECTED_WINDOWS_PUBLISHER`.
4. Enable immutable GitHub Releases for the repository before creating any
   candidate. Configure the read-only `RELEASE_ADMIN_READ_TOKEN` described in
   `CODE_SIGNING.md`; the signed-release and stable-publication workflows use it
   to check the live repository setting before privileged work. Both workflows
   also require each published release to read back as immutable.
5. Keep a recoverable administrator copy of the Tauri updater signing key. The
   public key in `src-tauri/tauri.conf.json` must match it; the release workflow
   verifies that relationship before publishing.
6. Before an initial baseline run, generate and retain a one-time random
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
   powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tests\restore-harness.tests.ps1
   powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tests\release-policy.tests.ps1
   npm run tauri -- build --no-bundle --ci
   npm run tauri -- bundle --bundles nsis --no-sign --ci
   ```

3. Merge through the protected branch and require both CI jobs green.
4. From the `main` branch in GitHub Actions, run **Signed Windows Release** with
   the final version. The workflow can publish only a GitHub prerelease
   candidate under the deterministic tag
   `v<version>-candidate.<full-40-character-main-SHA>`; it rejects SemVer
   prerelease suffixes and cannot publish a stable or latest release directly.
   For the initial `0.1.0` release, also set
   `build_initial_acceptance_baseline=true` and
   `acceptance_baseline_version=0.0.9`. The workflow refuses a moving branch,
   duplicate/non-monotonic version, missing signing value, unexpected
   publisher, invalid updater signature, incomplete asset set, remote asset
   digest mismatch, tag conflict, disabled release immutability, or partial
   publication. It creates the lightweight candidate tag only if that exact ref
   is absent, uploads a private draft, and rechecks the tag, `main`, immutable
   policy, and case-sensitive remote bytes before publication. It publishes
   these exact candidate assets:

   ```text
   Pusula_<version>_x64_offline-setup.exe
   Pusula_<version>_x64-setup.exe
   Pusula_<version>_x64.nsis.zip
   Pusula_<version>_x64.nsis.zip.sig
   latest.json
   SHA256SUMS.txt
   ```

A failed publication remains a draft. Do not rerun over it. Inspect the draft,
preserve the logs, and delete the draft/tag only after proving no asset was
released to users. The optional baseline is uploaded only as the encrypted,
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
   $installedExe = (Get-Process -Name 'pusula-desktop' -ErrorAction Stop |
     Select-Object -First 1).Path
   $productVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo($installedExe).ProductVersion
   if ($productVersion -ne $expectedVersion) { throw "Installed version is $productVersion." }
   $signature = Get-AuthenticodeSignature -LiteralPath $installedExe
   $publisher = $signature.SignerCertificate.GetNameInfo(
     [Security.Cryptography.X509Certificates.X509NameType]::SimpleName,
     $false
   )
   if ($signature.Status -ne 'Valid' -or -not $signature.TimeStamperCertificate -or
       $publisher -cne $expectedPublisher) {
     throw "Installed Pusula signature gate failed: $($signature.Status), $publisher"
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

Record only non-sensitive evidence. Save its final evidence JSON or worksheet
outside the repository and calculate:

```powershell
Get-FileHash -Algorithm SHA256 -LiteralPath 'C:\secure\pusula-acceptance-evidence.json'
```

## Publish stable without rebuilding

Do not merge another commit while a candidate is under acceptance. After every
acceptance item passes, run **Publish Accepted Windows Release** from the exact
candidate commit on `main` with:

- the exact candidate version;
- its exact immutable `v<version>-candidate.<40-character-SHA>` tag;
- the acceptance evidence SHA-256;
- confirmation `PROMOTE v<version>`.

The workflow requires immutable releases to be enabled, requires the candidate
itself to report `isImmutable=true`, and proves its tag and release target equal
the current `main` commit. It downloads the candidate, revalidates the exact
asset allowlist, SHA-256 values, Tauri updater signature, Authenticode
publisher, and timestamps, then creates a new draft stable tag `v<version>` at
that same commit. The tag is created exclusively as a lightweight ref, so a
concurrent or pre-existing tag fails closed. It uploads the six accepted files
byte-for-byte, then uses the read-only admin token to recheck immutable policy,
candidate identity, both exact refs, and case-sensitive remote digests before
the isolated write-token step publishes stable as latest. The workflow reads
back the immutable release, tag, and exact bytes after publication. It does not
rebuild, resign, or edit the immutable candidate.

The copied stable `latest.json` intentionally keeps its updater URL pointed at
the immutable candidate tag. The stable release serves that same manifest
through `/releases/latest`, while the signed updater payload remains anchored to
the exact candidate bytes exercised during acceptance. Keep both immutable
releases permanently.

If acceptance fails, leave the candidate as a prerelease, diagnose it, and
release a strictly greater version. Never replace a published candidate asset.
