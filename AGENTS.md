# Pusula Desktop Local Instructions

The parent `Documents/GitHub/AGENTS.md` rules also apply.

## Product Invariants

- The SQLite database on one Windows PC is the production source of truth.
- Customer, sale, installment, payment, report, and receipt workflows must work with the network physically disconnected.
- Network failures in updates, diagnostics, or backups must never block local business writes.
- Store money as integer kuruş in SQLite and convert only at the compatibility/UI boundary.
- Keep multi-step business writes transactional. A sale and its installments must either all commit or all roll back.
- Preserve imported legacy IDs and relationships.
- Do not add multi-machine synchronization or WordPress write-back.

## Security and Secrets

- Never commit updater private keys, Authenticode credentials, Backblaze keys, enrollment codes, device tokens, recovery private keys, production exports, or customer data.
- The desktop app must not receive Backblaze credentials. It may receive only a short-lived single-object upload URL.
- Example configuration files must contain placeholders only.

## Required Validation

Before committing behavior changes, run the relevant subset and record it in the PR:

```powershell
npm run build
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo test --manifest-path src-tauri/Cargo.toml
```

When `gateway/` changes, also run:

```powershell
cargo fmt --manifest-path gateway/Cargo.toml --check
cargo clippy --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path gateway/Cargo.toml
```

Changes to installation, updates, migrations, backups, or restore behavior must update the corresponding runbook and tests in the same commit.

## Release Gate

An initial release is not ready until a clean Windows profile can install it, import a fixture export, complete the offline workflow drill, update from the previous signed build, upload an encrypted backup, and restore that backup with matching row counts and financial totals.
