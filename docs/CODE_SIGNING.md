# Windows code signing

Pusula releases use Microsoft Azure Artifact Signing with a **Public Trust** certificate profile. The private key remains in Microsoft's managed hardware security modules; the release workflow does not import an Authenticode PFX or store a code-signing private key in GitHub.

This is separate from Tauri updater signing. The existing
`TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` secrets
sign the direct NSIS updater payload and produce its detached `.exe.sig` file;
they must remain configured.

## One-time Azure enrollment

An authorized representative of the legal publisher must complete these steps. Identity validation can be completed only in the Azure portal and may require email verification, government-issued identification, and current business records.

1. Use a paid Azure subscription and register the `Microsoft.CodeSigning` resource provider.
2. Create a Basic Artifact Signing account in a supported region.
3. Assign the representative the `Artifact Signing Identity Verifier` role and complete Public Trust organization identity validation.
4. After validation succeeds, create a Public Trust certificate profile.
5. Create a Microsoft Entra app registration for the GitHub release workflow.
6. Add a federated identity credential with:
   - Issuer: `https://token.actions.githubusercontent.com`
   - Audience: `api://AzureADTokenExchange`
   - Subject: `repo:stronganchor/pusula-desktop:environment:windows-release`
7. Assign the app registration the `Artifact Signing Certificate Profile Signer` role at the certificate-profile scope only:

   ```text
   /subscriptions/<subscription-id>/resourceGroups/<resource-group>/providers/Microsoft.CodeSigning/codeSigningAccounts/<account>/certificateProfiles/<profile>
   ```

Do not create an Entra client secret for this workflow. GitHub obtains a short-lived Azure token through OpenID Connect (OIDC).

Public Trust enrollment is currently available to organizations in the United States, Canada, the European Union, and the United Kingdom, and to individual developers in the United States and Canada. Microsoft documents a normal identity-validation window of 1 to 20 business days, potentially longer when more documentation is required.

As of 2026-07-14, Microsoft lists the Basic tier at USD 9.99 per month for up
to 5,000 signatures, bills the full monthly amount rather than prorating it,
and starts billing when the Artifact Signing account is created. Obtain the
publisher's explicit spending approval immediately before creating the Azure
resource, and re-check the live pricing page because these terms can change.

Microsoft's setup documentation:

- <https://learn.microsoft.com/azure/artifact-signing/quickstart>
- <https://learn.microsoft.com/azure/artifact-signing/tutorial-assign-roles>
- <https://learn.microsoft.com/azure/developer/github/connect-from-azure-openid-connect>
- <https://azure.microsoft.com/pricing/details/artifact-signing/>

## GitHub release environment

Create and protect a GitHub environment named `windows-release`. Restrict deployments to the `main` branch and require an appropriate reviewer before a signing job starts.

Configure these environment variables (not secrets):

| Variable | Value |
| --- | --- |
| `AZURE_CLIENT_ID` | Application/client ID of the Entra app registration |
| `AZURE_TENANT_ID` | Entra tenant ID |
| `AZURE_SUBSCRIPTION_ID` | Azure subscription containing Artifact Signing |
| `ARTIFACT_SIGNING_ENDPOINT` | Regional endpoint, for example `https://eus.codesigning.azure.net` |
| `ARTIFACT_SIGNING_ACCOUNT` | Artifact Signing account name |
| `ARTIFACT_SIGNING_PROFILE` | Public Trust certificate profile name |
| `EXPECTED_WINDOWS_PUBLISHER` | Exact publisher name returned by the profile's Authenticode certificate |

Configure these environment secrets:

| Secret | Purpose |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | Tauri updater private key |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Password for the updater private key |
| `ACCEPTANCE_BASELINE_ARCHIVE_PASSWORD` | One-time 24+ character password retained by the acceptance operator; required only when building the initial private baseline |
| `RELEASE_ADMIN_READ_TOKEN` | Fine-grained token restricted to this repository with Administration read and Contents read; used only to prove immutable releases are enabled and inspect the candidate |

Give `RELEASE_ADMIN_READ_TOKEN` an expiration, rotate it before expiry, and do
not grant write permission. The obsolete `WINDOWS_CODESIGN_PFX_BASE64` and
`WINDOWS_CODESIGN_PASSWORD` secrets must not be configured or used.

## Release behavior

The workflow first validates and compiles the immutable `main` commit without
Azure access, updater secrets, or a write-capable GitHub token. Before signing,
the protected read-only administration token proves repository release
immutability is enabled. The signing job then downloads the official Microsoft
Artifact Signing Client Tools MSI,
verifies Microsoft's Authenticode signature, and installs it noninteractively
before logging in with `azure/login` and GitHub OIDC. The updater private key is
available only to the preflight validation and candidate bundle steps. Tauri
invokes `scripts/Invoke-ArtifactSigning.ps1` through its custom `signCommand` for every
Windows executable it bundles.

The helper uses SHA-256 and Microsoft's RFC 3161 timestamp service. It authenticates only through the Azure CLI session created by `azure/login`; interactive and unrelated credential providers are explicitly disabled. A signing or verification failure stops the build.

Signing happens during Tauri bundling so both the inner `pusula-desktop.exe` and
each NSIS setup executable receive Authenticode signatures. Tauri v2 then uses
the Authenticode-signed lean installer itself,
`Pusula_<version>_x64-setup.exe`, as the updater payload and emits its detached
Tauri signature as `Pusula_<version>_x64-setup.exe.sig`; no updater ZIP is
created or extracted. The larger
`Pusula_<version>_x64_offline-setup.exe` remains a separate disconnected-install
artifact with the offline WebView2 runtime and is not the in-app updater
payload. The optional initial-release acceptance baseline is freshly compiled
with its own lower embedded version and direct candidate-manifest endpoint
before Azure authentication, then signed separately. Because the repository is
public, the workflow encrypts that installer as a header-encrypted AES-256
7-Zip archive before uploading a three-day Actions artifact and deletes the
plaintext runner copies. The release gate requires the exact
`EXPECTED_WINDOWS_PUBLISHER`, a valid timestamp, and a Tauri signature over the
exact Authenticode-signed lean installer that verifies against the public key
embedded in the application. It verifies:

- `Pusula_<version>_x64_offline-setup.exe`
- `Pusula_<version>_x64-setup.exe`
- `Pusula_<version>_x64-setup.exe.sig`

Tauri restores the unsigned build-tree executable after it packages each target,
so that restored file is not used as signature evidence. Acceptance instead
verifies the installed `pusula-desktop.exe` after both the baseline install and
candidate update; that proves the inner application extracted from the signed
NSIS package has the expected publisher and timestamp.

Verified artifacts cross into a separate publication job without the updater
private key or Azure session. That job exclusively creates a lightweight
candidate tag derived from the final version and full commit SHA, creates a
draft, and uploads only the allowlisted artifacts and SHA-256 manifest. A
read-only admin token then rechecks repository immutability, `main`, the exact
tag ref, draft state, and case-sensitive remote names/sizes/digests immediately
before the isolated write-token step publishes it. The job requires the tag,
release state, and exact bytes to read back correctly after the candidate is
immutable. After acceptance, the stable-publication workflow applies the same
gates while copying those exact files under a new immutable `v<version>` stable
release; it never edits the immutable candidate. The release remains blocked
until Azure identity validation is complete, the Public Trust profile exists,
the expected publisher value is configured, the GitHub environment is
protected, and a workflow run proves every signature valid. Candidate testing
and stable publication are defined in `RELEASE_RUNBOOK.md`.
