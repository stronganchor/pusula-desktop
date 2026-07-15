# Windows distribution and updater signing

Pusula uses a managed single-machine Windows distribution model for the
initial release:

- the NSIS installers are intentionally **not Authenticode signed**;
- installation is per-user (`currentUser`) and does not require administrator
  rights;
- every in-app updater payload is signed with the Pusula Tauri updater key;
- the Tauri public key is embedded in the application and update signature
  verification cannot be disabled;
- GitHub immutable releases, protected `v*` tags, exact asset hashes, and
  canonical acceptance evidence bind the distributed bytes to the release.

This removes the Azure Artifact Signing enrollment, billing, organization
identity, protected-environment reviewer, and administration-token setup that
would be disproportionate for one managed computer.

## Two different kinds of signature

Authenticode identifies a Windows publisher to Windows and can affect the text
shown in download and SmartScreen prompts. It is not required to run a Windows
application. Pusula's initial installers must report `NotSigned` from
`Get-AuthenticodeSignature`; an unexpected Authenticode signature fails the
release.

The Tauri updater signature authenticates the exact updater installer before
Pusula can run it. Tauri requires this signature and validates it with the
public key in `src-tauri/tauri.conf.json`. Losing or replacing the corresponding
private key would prevent future updates for an installed copy.

The first installer is downloaded through a browser, so Windows may show
**Windows protected your PC** and **Unknown publisher**. The managed installer
procedure requires one explicit acknowledgement after its filename and
SHA-256 have been verified. In-app updates are downloaded and verified by
Pusula, use a current-user passive NSIS install, and must complete without
another manual Windows prompt in acceptance testing.

References:

- <https://v2.tauri.app/plugin/updater/>
- <https://v2.tauri.app/distribute/sign/windows/>
- <https://learn.microsoft.com/windows/apps/package-and-deploy/smartscreen-reputation>

## Why not use a self-signed Authenticode certificate

A self-signed certificate is not trusted by Windows on a new computer and
receives the same strong first-download SmartScreen treatment as an unsigned
file. Making it useful would require installing a permanent local trust anchor,
protecting another private key, and maintaining another signing process. It
would not replace the Tauri updater signature. For one machine, that increases
risk and work without improving the initial install.

Do not install a Pusula certificate in Trusted Root or Trusted Publishers. If
the deployment later expands beyond the managed machine, replace this policy
with a publicly trusted Authenticode or Microsoft Store distribution before
shipping to those users.

## Existing protected values

The GitHub environment `windows-release` is restricted to `main` and contains:

| Secret | Purpose |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | Signs the direct NSIS updater payload |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Unlocks that updater key during packaging |
| `ACCEPTANCE_BASELINE_ARCHIVE_PASSWORD` | Encrypts the temporary private 0.0.9 acceptance baseline |

No Azure values, Windows certificate, reviewer, or separate personal access
token is required.

The recoverable local updater material is stored outside the repository at:

```text
%USERPROFILE%\.pusula-secrets\tauri-updater.key
%USERPROFILE%\.pusula-secrets\tauri-updater.key.pub
%USERPROFILE%\.pusula-secrets\tauri-updater-password.dpapi
```

The directory ACL must grant access only to the owning Windows user. The public
file must exactly equal `plugins.updater.pubkey` in
`src-tauri/tauri.conf.json`. The older
`%USERPROFILE%\.tauri\pusula-desktop.key` belongs to a superseded public key
and must not be used for a release. Never print, commit, upload as an artifact,
or copy the private key/password into logs.

## Release workflow

`.github/workflows/release.yml` performs these operations:

1. Pin the workflow to the exact `main` commit and validate SemVer/release
   identity.
2. Run desktop, frontend, restore, release-policy, and gateway tests.
3. Compile the candidate and, for the first release only, a private 0.0.9
   baseline that points to the exact candidate tag.
4. Build the full offline installer with `--no-sign`.
5. Build the lean updater with `TAURI_SIGNING_PRIVATE_KEY`; Tauri emits the
   direct `.exe.sig`.
6. Require `NotSigned` Authenticode status on the offline, updater, and
   baseline installers.
7. Verify the updater signature independently with pinned Minisign, generate
   `latest.json` and `SHA256SUMS.txt`, and publish an immutable prerelease
   candidate.
8. Keep the baseline in an encrypted, three-day Actions artifact; it is never a
   GitHub release asset.

The publication job receives no updater private key. It validates numeric
release/asset identities, exact names, sizes, SHA-256 digests, tag/commit
identity, and the immutable readback before accepting publication.

`.github/workflows/promote-release.yml` downloads the immutable candidate,
revalidates the Tauri signature and intentional `NotSigned` status, validates
canonical clean-machine acceptance evidence, and creates the immutable stable
release. GitHub's job-scoped token is sufficient; no separate personal token
or second reviewer is required.

## Release invariants

Never publish when any of these are false:

- the updater `.exe.sig` verifies against the embedded public key;
- the offline and updater installers both report Authenticode `NotSigned`;
- the updater manifest carries the exact detached signature text and immutable
  release URL;
- the candidate tag points to the exact tested `main` commit;
- all candidate assets match `SHA256SUMS.txt`;
- the clean standard-user drill acknowledges SmartScreen only for the initial
  browser-downloaded installer;
- the baseline-to-candidate update completes with zero manual certificate
  installation, exactly one Pusula confirmation, and no Windows/SmartScreen
  prompt;
- the invalid-signature runtime drill rejects a tampered updater before
  installation confirmation;
- encrypted backup, independent gateway storage readback, and restore evidence
  pass.

Signing the updater is necessary but does not authorize publishing it. The
candidate and stable publication gates remain separate.
