use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{
        BusinessProfile, ContactExport, CustomerExport, DatabaseStatus, ExportBundle,
        ExportManifest, ExportSummary, FinancialTotals, ImportSummary, InstallmentExport,
        PaymentExport, RecordCounts, SaleExport,
    },
};

pub const SCHEMA_VERSION: i32 = 1;
pub const EXPORT_FORMAT_VERSION: u32 = 1;
pub(crate) const MAX_SAFE_JS_INTEGER: i64 = 9_007_199_254_740_991;

const MIGRATION_1: &str = r#"
CREATE TABLE business_profile (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    name            TEXT NOT NULL DEFAULT '',
    address         TEXT NOT NULL DEFAULT '',
    phone           TEXT NOT NULL DEFAULT '',
    website         TEXT NOT NULL DEFAULT '',
    footer_sub      TEXT NOT NULL DEFAULT ''
);

CREATE TABLE customers (
    id                  INTEGER PRIMARY KEY,
    name                TEXT NOT NULL CHECK (length(trim(name)) > 0),
    phone               TEXT NOT NULL DEFAULT '',
    address             TEXT NOT NULL DEFAULT '',
    work_address        TEXT NOT NULL DEFAULT '',
    notes               TEXT NOT NULL DEFAULT '',
    registration_date   TEXT NOT NULL
);

CREATE INDEX customers_name_idx ON customers(name COLLATE NOCASE);
CREATE INDEX customers_registration_idx ON customers(registration_date DESC, id DESC);

CREATE TABLE contacts (
    id              INTEGER PRIMARY KEY,
    customer_id     INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    name            TEXT NOT NULL DEFAULT '',
    phone           TEXT NOT NULL DEFAULT '',
    home_address    TEXT NOT NULL DEFAULT '',
    work_address    TEXT NOT NULL DEFAULT ''
);

CREATE INDEX contacts_customer_idx ON contacts(customer_id, id);

CREATE TABLE sales (
    id              INTEGER PRIMARY KEY,
    customer_id     INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    date            TEXT NOT NULL,
    total_kurus     INTEGER NOT NULL CHECK (total_kurus >= 0),
    description     TEXT NOT NULL DEFAULT '',
    request_key     TEXT UNIQUE
);

CREATE INDEX sales_customer_idx ON sales(customer_id, date DESC, id DESC);
CREATE INDEX sales_date_idx ON sales(date DESC, id DESC);

CREATE TABLE installments (
    id              INTEGER PRIMARY KEY,
    sale_id         INTEGER NOT NULL REFERENCES sales(id) ON DELETE CASCADE,
    due_date        TEXT,
    amount_kurus    INTEGER NOT NULL CHECK (amount_kurus >= 0),
    paid_date       TEXT
);

CREATE INDEX installments_sale_idx ON installments(sale_id, due_date, id);
CREATE INDEX installments_due_idx ON installments(due_date, id);

CREATE TABLE installment_payments (
    id                  INTEGER PRIMARY KEY,
    installment_id      INTEGER NOT NULL REFERENCES installments(id) ON DELETE CASCADE,
    amount_kurus        INTEGER NOT NULL CHECK (amount_kurus > 0),
    payment_date        TEXT NOT NULL,
    created_at          TEXT NOT NULL
);

CREATE INDEX payments_installment_idx
    ON installment_payments(installment_id, payment_date, id);
CREATE INDEX payments_date_idx ON installment_payments(payment_date, id);

CREATE TABLE settings (
    key             TEXT PRIMARY KEY,
    value           TEXT NOT NULL
);

INSERT INTO business_profile(id) VALUES (1);
"#;

#[derive(Debug, Clone)]
pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn initialize(path: impl Into<PathBuf>) -> AppResult<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut connection = Connection::open(&path)?;
        configure_connection(&connection)?;
        migrate(&mut connection)?;
        ensure_database_settings(&mut connection)?;

        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn connect(&self) -> AppResult<Connection> {
        let connection = Connection::open(&self.path)?;
        configure_connection(&connection)?;
        Ok(connection)
    }

    pub fn business_profile(&self) -> AppResult<BusinessProfile> {
        let connection = self.connect()?;
        read_business_profile(&connection)
    }

    pub fn export_data(&self) -> AppResult<ExportBundle> {
        self.export_data_with_hook(|| {})
    }

    fn export_data_with_hook<F>(&self, after_snapshot_started: F) -> AppResult<ExportBundle>
    where
        F: FnOnce(),
    {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let business_profile = read_business_profile(&transaction)?;

        // The first read above establishes the WAL snapshot. This hook is a
        // deterministic test seam for proving that later table reads cannot
        // observe a concurrent commit from a newer database state.
        after_snapshot_started();

        let customers = query_all(&transaction, "SELECT id, name, phone, address, work_address, notes, registration_date FROM customers ORDER BY id", |row| {
            Ok(CustomerExport {
                id: row.get(0)?,
                name: row.get(1)?,
                phone: row.get(2)?,
                address: row.get(3)?,
                work_address: row.get(4)?,
                notes: row.get(5)?,
                registration_date: row.get(6)?,
            })
        })?;

        let contacts = query_all(&transaction, "SELECT id, customer_id, name, phone, home_address, work_address FROM contacts ORDER BY id", |row| {
            Ok(ContactExport {
                id: row.get(0)?,
                customer_id: row.get(1)?,
                name: row.get(2)?,
                phone: row.get(3)?,
                home_address: row.get(4)?,
                work_address: row.get(5)?,
            })
        })?;

        let sales = query_all(&transaction, "SELECT id, customer_id, date, total_kurus, description, request_key FROM sales ORDER BY id", |row| {
            Ok(SaleExport {
                id: row.get(0)?,
                customer_id: row.get(1)?,
                date: row.get(2)?,
                total_kurus: row.get(3)?,
                description: row.get(4)?,
                request_key: row.get(5)?,
            })
        })?;

        let installments = query_all(
            &transaction,
            "SELECT id, sale_id, due_date, amount_kurus, paid_date FROM installments ORDER BY id",
            |row| {
                Ok(InstallmentExport {
                    id: row.get(0)?,
                    sale_id: row.get(1)?,
                    due_date: row.get(2)?,
                    amount_kurus: row.get(3)?,
                    paid_date: row.get(4)?,
                })
            },
        )?;

        let payments = query_all(&transaction, "SELECT id, installment_id, amount_kurus, payment_date, created_at FROM installment_payments ORDER BY id", |row| {
            Ok(PaymentExport {
                id: row.get(0)?,
                installment_id: row.get(1)?,
                amount_kurus: row.get(2)?,
                payment_date: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;

        let counts = RecordCounts {
            customers: customers.len(),
            contacts: contacts.len(),
            sales: sales.len(),
            installments: installments.len(),
            payments: payments.len(),
        };
        let totals = calculate_totals(&sales, &installments, &payments)?;

        let mut bundle = ExportBundle {
            format_version: EXPORT_FORMAT_VERSION,
            source: "pusula-desktop".to_owned(),
            source_version: env!("CARGO_PKG_VERSION").to_owned(),
            exported_at: Local::now().to_rfc3339(),
            business_profile,
            customers,
            contacts,
            sales,
            installments,
            payments,
            manifest: ExportManifest {
                counts,
                totals,
                sha256: String::new(),
            },
        };
        bundle.manifest.sha256 = bundle_checksum(&bundle)?;
        validate_bundle(&bundle)?;
        transaction.commit()?;
        Ok(bundle)
    }

    pub fn import_data(&self, bundle: ExportBundle, replace: bool) -> AppResult<ImportSummary> {
        validate_bundle(&bundle)?;
        let summary = ImportSummary {
            replaced: replace,
            counts: bundle.manifest.counts.clone(),
            totals: bundle.manifest.totals.clone(),
            sha256: bundle.manifest.sha256.clone(),
        };
        let serialized_summary = serde_json::to_string(&summary)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;

        if replace {
            transaction.execute("DELETE FROM customers", [])?;
        }

        write_business_profile(&transaction, &bundle.business_profile)?;

        for customer in &bundle.customers {
            transaction.execute(
                "INSERT INTO customers(id, name, phone, address, work_address, notes, registration_date) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![customer.id, customer.name, customer.phone, customer.address, customer.work_address, customer.notes, customer.registration_date],
            )?;
        }
        for contact in &bundle.contacts {
            transaction.execute(
                "INSERT INTO contacts(id, customer_id, name, phone, home_address, work_address) VALUES (?, ?, ?, ?, ?, ?)",
                params![contact.id, contact.customer_id, contact.name, contact.phone, contact.home_address, contact.work_address],
            )?;
        }
        for sale in &bundle.sales {
            transaction.execute(
                "INSERT INTO sales(id, customer_id, date, total_kurus, description, request_key) VALUES (?, ?, ?, ?, ?, ?)",
                params![sale.id, sale.customer_id, sale.date, sale.total_kurus, sale.description, sale.request_key],
            )?;
        }
        for installment in &bundle.installments {
            transaction.execute(
                "INSERT INTO installments(id, sale_id, due_date, amount_kurus, paid_date) VALUES (?, ?, ?, ?, ?)",
                params![installment.id, installment.sale_id, installment.due_date, installment.amount_kurus, installment.paid_date],
            )?;
        }
        for payment in &bundle.payments {
            transaction.execute(
                "INSERT INTO installment_payments(id, installment_id, amount_kurus, payment_date, created_at) VALUES (?, ?, ?, ?, ?)",
                params![payment.id, payment.installment_id, payment.amount_kurus, payment.payment_date, payment.created_at],
            )?;
        }

        // paid_date is derived from immutable payment rows. Recompute it instead
        // of trusting a legacy export that may have stale flags.
        transaction.execute_batch(
            "UPDATE installments
             SET paid_date = CASE
                 WHEN COALESCE((SELECT SUM(p.amount_kurus) FROM installment_payments p WHERE p.installment_id = installments.id), 0) >= amount_kurus
                 THEN (SELECT p.payment_date FROM installment_payments p WHERE p.installment_id = installments.id ORDER BY p.payment_date DESC, p.id DESC LIMIT 1)
                 ELSE NULL
             END;",
        )?;
        write_setting(&transaction, "last_import", &serialized_summary)?;
        write_setting(&transaction, "onboarding_complete", "true")?;
        mark_modified(&transaction)?;
        transaction.commit()?;

        Ok(summary)
    }

    pub fn export_data_file(&self, path: &Path, overwrite: bool) -> AppResult<ExportSummary> {
        validate_transfer_path(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| AppError::user("Aktarım hedef klasörü geçersiz."))?;
        if !parent.is_dir() {
            return Err(AppError::user("Aktarım hedef klasörü bulunamadı."));
        }
        if path.exists() && !overwrite {
            return Err(AppError::user(
                "Hedef dosya zaten var. Üzerine yazmayı onaylayın veya başka ad seçin.",
            ));
        }

        let bundle = self.export_data()?;
        let bytes = serde_json::to_vec_pretty(&bundle)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;

        if overwrite {
            temporary
                .persist(path)
                .map_err(|error| AppError::Io(error.error))?;
        } else {
            temporary
                .persist_noclobber(path)
                .map_err(|error| AppError::Io(error.error))?;
        }

        Ok(ExportSummary {
            path: path.to_string_lossy().into_owned(),
            bytes_written: u64::try_from(bytes.len())
                .map_err(|_| AppError::user("Aktarım dosyası çok büyük."))?,
            counts: bundle.manifest.counts,
            totals: bundle.manifest.totals,
            sha256: bundle.manifest.sha256,
        })
    }

    pub fn import_data_file(&self, path: &Path, replace: bool) -> AppResult<ImportSummary> {
        validate_transfer_path(path)?;
        let metadata = fs::metadata(path)
            .map_err(|_| AppError::user("Aktarım dosyası bulunamadı veya okunamıyor."))?;
        const MAX_IMPORT_BYTES: u64 = 256 * 1024 * 1024;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_IMPORT_BYTES {
            return Err(AppError::user(
                "Aktarım dosyası boş veya desteklenen boyuttan büyük.",
            ));
        }
        let bytes = fs::read(path)?;
        let bundle: ExportBundle = serde_json::from_slice(&bytes)?;
        self.import_data(bundle, replace)
    }

    pub fn acknowledge_empty_start(&self) -> AppResult<()> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        write_setting(&transaction, "onboarding_complete", "true")?;
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn status(&self) -> AppResult<DatabaseStatus> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let schema_version =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let journal_mode =
            transaction.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let integrity_check =
            transaction.pragma_query_value(None, "integrity_check", |row| row.get(0))?;
        let database_id = read_required_setting(&transaction, "database_id")?;
        Uuid::parse_str(&database_id)
            .map_err(|_| AppError::user("Veritabanı kimliği geçersiz."))?;
        let onboarding_complete = parse_setting_bool(
            &read_required_setting(&transaction, "onboarding_complete")?,
            "onboarding_complete",
        )?;
        let last_modified_at = read_optional_setting(&transaction, "last_modified_at")?;
        let last_import = read_optional_setting(&transaction, "last_import")?
            .map(|value| serde_json::from_str::<ImportSummary>(&value))
            .transpose()?;
        if let Some(summary) = &last_import {
            validate_import_summary(summary)?;
        }
        let counts = counts_from_database(&transaction)?;
        let totals = totals_from_database(&transaction)?;

        let status = DatabaseStatus {
            path: self.path.to_string_lossy().into_owned(),
            database_id,
            schema_version,
            journal_mode,
            integrity_check,
            last_modified_at,
            onboarding_complete,
            last_import,
            counts,
            totals,
        };
        transaction.commit()?;
        Ok(status)
    }
}

fn validate_transfer_path(path: &Path) -> AppResult<()> {
    if !path.is_absolute() {
        return Err(AppError::user(
            "Aktarım dosyası tam Windows yolu olmalıdır.",
        ));
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !extension.eq_ignore_ascii_case("json") {
        return Err(AppError::user(
            "Aktarım dosyasının uzantısı .json olmalıdır.",
        ));
    }
    Ok(())
}

fn configure_connection(connection: &Connection) -> AppResult<()> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA synchronous = FULL;
         PRAGMA journal_mode = WAL;",
    )?;
    Ok(())
}

fn migrate(connection: &mut Connection) -> AppResult<()> {
    let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(AppError::user(format!(
            "Veritabanı sürümü ({version}) bu uygulamadan daha yeni. Uygulamayı güncelleyin."
        )));
    }

    if version < 1 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(MIGRATION_1)?;
        transaction.pragma_update(None, "user_version", 1)?;
        transaction.commit()?;
    }
    Ok(())
}

fn ensure_database_settings(connection: &mut Connection) -> AppResult<()> {
    let transaction = connection.transaction()?;
    let database_id = read_optional_setting(&transaction, "database_id")?;
    match database_id {
        Some(value) => {
            Uuid::parse_str(&value).map_err(|_| AppError::user("Veritabanı kimliği geçersiz."))?;
        }
        None => write_setting(&transaction, "database_id", &Uuid::new_v4().to_string())?,
    }

    let onboarding = read_optional_setting(&transaction, "onboarding_complete")?;
    match onboarding {
        Some(value) => {
            parse_setting_bool(&value, "onboarding_complete")?;
        }
        None => {
            let initialized = database_has_user_data(&transaction)?;
            write_setting(
                &transaction,
                "onboarding_complete",
                if initialized { "true" } else { "false" },
            )?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn database_has_user_data(connection: &Connection) -> AppResult<bool> {
    let initialized: i64 = connection.query_row(
        "SELECT
            EXISTS(SELECT 1 FROM customers LIMIT 1)
            OR EXISTS(SELECT 1 FROM contacts LIMIT 1)
            OR EXISTS(SELECT 1 FROM sales LIMIT 1)
            OR EXISTS(SELECT 1 FROM installments LIMIT 1)
            OR EXISTS(SELECT 1 FROM installment_payments LIMIT 1)
            OR EXISTS(
                SELECT 1 FROM business_profile
                WHERE id = 1 AND (
                    trim(name) <> '' OR trim(address) <> '' OR trim(phone) <> ''
                    OR trim(website) <> '' OR trim(footer_sub) <> ''
                )
                LIMIT 1
            )",
        [],
        |row| row.get(0),
    )?;
    Ok(initialized != 0)
}

fn read_optional_setting(connection: &Connection, key: &str) -> AppResult<Option<String>> {
    Ok(connection
        .query_row("SELECT value FROM settings WHERE key = ?", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn read_required_setting(connection: &Connection, key: &str) -> AppResult<String> {
    read_optional_setting(connection, key)?
        .ok_or_else(|| AppError::user(format!("Zorunlu veritabanı ayarı eksik: {key}.")))
}

fn write_setting(connection: &Connection, key: &str, value: &str) -> AppResult<()> {
    connection.execute(
        "INSERT INTO settings(key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn parse_setting_bool(value: &str, key: &str) -> AppResult<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(AppError::user(format!("Veritabanı ayarı geçersiz: {key}."))),
    }
}

pub(crate) fn mark_modified(connection: &Connection) -> AppResult<()> {
    write_setting(connection, "last_modified_at", &Local::now().to_rfc3339())
}

pub(crate) fn read_business_profile(connection: &Connection) -> AppResult<BusinessProfile> {
    Ok(connection.query_row(
        "SELECT name, address, phone, website, footer_sub FROM business_profile WHERE id = 1",
        [],
        |row| {
            Ok(BusinessProfile {
                name: row.get(0)?,
                address: row.get(1)?,
                phone: row.get(2)?,
                website: row.get(3)?,
                footer_sub: row.get(4)?,
            })
        },
    )?)
}

pub(crate) fn write_business_profile(
    connection: &Connection,
    profile: &BusinessProfile,
) -> AppResult<()> {
    connection.execute(
        "INSERT INTO business_profile(id, name, address, phone, website, footer_sub)
         VALUES (1, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             name = excluded.name,
             address = excluded.address,
             phone = excluded.phone,
             website = excluded.website,
             footer_sub = excluded.footer_sub",
        params![
            profile.name,
            profile.address,
            profile.phone,
            profile.website,
            profile.footer_sub
        ],
    )?;
    Ok(())
}

fn query_all<T, F>(connection: &Connection, sql: &str, map: F) -> AppResult<Vec<T>>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], map)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn count_table(connection: &Connection, table: &str) -> AppResult<usize> {
    // `table` is only called with constants in this module.
    let count: i64 = connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })?;
    usize::try_from(count).map_err(|_| AppError::user("Geçersiz kayıt sayısı."))
}

fn counts_from_database(connection: &Connection) -> AppResult<RecordCounts> {
    Ok(RecordCounts {
        customers: count_table(connection, "customers")?,
        contacts: count_table(connection, "contacts")?,
        sales: count_table(connection, "sales")?,
        installments: count_table(connection, "installments")?,
        payments: count_table(connection, "installment_payments")?,
    })
}

fn totals_from_database(connection: &Connection) -> AppResult<FinancialTotals> {
    let totals = FinancialTotals {
        sales_kurus: connection.query_row(
            "SELECT COALESCE(SUM(total_kurus), 0) FROM sales",
            [],
            |row| row.get(0),
        )?,
        installments_kurus: connection.query_row(
            "SELECT COALESCE(SUM(amount_kurus), 0) FROM installments",
            [],
            |row| row.get(0),
        )?,
        payments_kurus: connection.query_row(
            "SELECT COALESCE(SUM(amount_kurus), 0) FROM installment_payments",
            [],
            |row| row.get(0),
        )?,
    };
    validate_safe_totals(&totals)?;
    Ok(totals)
}

fn checked_sum(mut values: impl Iterator<Item = i64>) -> AppResult<i64> {
    let total = values.try_fold(0_i64, |sum, value| {
        sum.checked_add(value)
            .ok_or_else(|| AppError::user("Para toplamı desteklenen aralığı aşıyor."))
    })?;
    validate_safe_integer(total, "Para toplamı")?;
    Ok(total)
}

fn calculate_totals(
    sales: &[SaleExport],
    installments: &[InstallmentExport],
    payments: &[PaymentExport],
) -> AppResult<FinancialTotals> {
    Ok(FinancialTotals {
        sales_kurus: checked_sum(sales.iter().map(|row| row.total_kurus))?,
        installments_kurus: checked_sum(installments.iter().map(|row| row.amount_kurus))?,
        payments_kurus: checked_sum(payments.iter().map(|row| row.amount_kurus))?,
    })
}

fn validate_safe_integer(value: i64, label: &str) -> AppResult<()> {
    if value.unsigned_abs() > MAX_SAFE_JS_INTEGER as u64 {
        return Err(AppError::user(format!(
            "{label} güvenli uygulama aralığını aşıyor."
        )));
    }
    Ok(())
}

fn validate_safe_totals(totals: &FinancialTotals) -> AppResult<()> {
    validate_safe_integer(totals.sales_kurus, "Satış toplamı")?;
    validate_safe_integer(totals.installments_kurus, "Taksit toplamı")?;
    validate_safe_integer(totals.payments_kurus, "Ödeme toplamı")?;
    Ok(())
}

fn validate_import_summary(summary: &ImportSummary) -> AppResult<()> {
    if summary.sha256.len() != 64
        || !summary
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AppError::user("Son içe aktarma özeti geçersiz."));
    }
    validate_safe_totals(&summary.totals)
}

pub fn bundle_checksum(bundle: &ExportBundle) -> AppResult<String> {
    let mut canonical = bundle.clone();
    canonical.manifest.sha256.clear();
    let bytes = serde_json::to_vec(&canonical)?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn validate_bundle(bundle: &ExportBundle) -> AppResult<()> {
    if bundle.format_version != EXPORT_FORMAT_VERSION {
        return Err(AppError::user(format!(
            "Desteklenmeyen aktarım biçimi: {}.",
            bundle.format_version
        )));
    }
    if bundle.source.trim().is_empty() || bundle.source_version.trim().is_empty() {
        return Err(AppError::user("Aktarım kaynağı veya sürümü eksik."));
    }
    DateTime::parse_from_rfc3339(&bundle.exported_at)
        .map_err(|_| AppError::user("Aktarım zamanı geçersiz."))?;

    if bundle.manifest.sha256.len() != 64
        || !bundle
            .manifest
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || bundle_checksum(bundle)? != bundle.manifest.sha256
    {
        return Err(AppError::user(
            "Aktarım dosyasının SHA-256 doğrulaması başarısız.",
        ));
    }

    let expected_counts = RecordCounts {
        customers: bundle.customers.len(),
        contacts: bundle.contacts.len(),
        sales: bundle.sales.len(),
        installments: bundle.installments.len(),
        payments: bundle.payments.len(),
    };
    if expected_counts != bundle.manifest.counts {
        return Err(AppError::user("Aktarım kayıt sayıları eşleşmiyor."));
    }
    let expected_totals = calculate_totals(&bundle.sales, &bundle.installments, &bundle.payments)?;
    if expected_totals != bundle.manifest.totals {
        return Err(AppError::user("Aktarım para toplamları eşleşmiyor."));
    }
    validate_safe_totals(&bundle.manifest.totals)?;

    validate_profile(&bundle.business_profile)?;

    let mut customer_ids = HashSet::new();
    for row in &bundle.customers {
        require_unique_positive_id(row.id, &mut customer_ids, "müşteri")?;
        if row.name.trim().is_empty() {
            return Err(AppError::user("Aktarımda adı boş müşteri var."));
        }
        validate_iso_date(&row.registration_date, "Müşteri kayıt tarihi")?;
    }

    let mut contact_ids = HashSet::new();
    for row in &bundle.contacts {
        require_unique_positive_id(row.id, &mut contact_ids, "iletişim")?;
        if !customer_ids.contains(&row.customer_id) {
            return Err(AppError::user(format!(
                "İletişim kaydı {} bilinmeyen müşteriye bağlı.",
                row.id
            )));
        }
    }

    let mut sale_ids = HashSet::new();
    let mut request_keys = HashSet::new();
    for row in &bundle.sales {
        require_unique_positive_id(row.id, &mut sale_ids, "satış")?;
        if !customer_ids.contains(&row.customer_id) {
            return Err(AppError::user(format!(
                "Satış {} bilinmeyen müşteriye bağlı.",
                row.id
            )));
        }
        validate_iso_date(&row.date, "Satış tarihi")?;
        if row.total_kurus < 0 || row.total_kurus > MAX_SAFE_JS_INTEGER {
            return Err(AppError::user(
                "Satış tutarı güvenli uygulama aralığında olmalıdır.",
            ));
        }
        if let Some(key) = row.request_key.as_deref() {
            if key.len() > 64 || !request_keys.insert(key.to_owned()) {
                return Err(AppError::user(
                    "Geçersiz veya yinelenen satış istek anahtarı.",
                ));
            }
        }
    }

    let mut installment_ids = HashSet::new();
    let mut installment_amounts = HashMap::new();
    for row in &bundle.installments {
        require_unique_positive_id(row.id, &mut installment_ids, "taksit")?;
        if !sale_ids.contains(&row.sale_id) {
            return Err(AppError::user(format!(
                "Taksit {} bilinmeyen satışa bağlı.",
                row.id
            )));
        }
        if let Some(date) = row.due_date.as_deref() {
            validate_iso_date(date, "Taksit vadesi")?;
        }
        if let Some(date) = row.paid_date.as_deref() {
            validate_iso_date(date, "Taksit ödeme tarihi")?;
        }
        if row.amount_kurus < 0 || row.amount_kurus > MAX_SAFE_JS_INTEGER {
            return Err(AppError::user(
                "Taksit tutarı güvenli uygulama aralığında olmalıdır.",
            ));
        }
        installment_amounts.insert(row.id, row.amount_kurus);
    }

    let mut payment_ids = HashSet::new();
    let mut payment_totals: HashMap<i64, i64> = HashMap::new();
    for row in &bundle.payments {
        require_unique_positive_id(row.id, &mut payment_ids, "ödeme")?;
        if !installment_ids.contains(&row.installment_id) {
            return Err(AppError::user(format!(
                "Ödeme {} bilinmeyen taksite bağlı.",
                row.id
            )));
        }
        if row.amount_kurus <= 0 || row.amount_kurus > MAX_SAFE_JS_INTEGER {
            return Err(AppError::user(
                "Ödeme tutarı güvenli uygulama aralığında olmalıdır.",
            ));
        }
        validate_iso_date(&row.payment_date, "Ödeme tarihi")?;
        if NaiveDateTime::parse_from_str(&row.created_at, "%Y-%m-%d %H:%M:%S").is_err()
            && DateTime::parse_from_rfc3339(&row.created_at).is_err()
        {
            return Err(AppError::user("Ödeme oluşturma zamanı geçersiz."));
        }
        let total = payment_totals.entry(row.installment_id).or_default();
        *total = total
            .checked_add(row.amount_kurus)
            .ok_or_else(|| AppError::user("Ödeme toplamı desteklenen aralığı aşıyor."))?;
    }
    for (installment_id, total) in payment_totals {
        if total > installment_amounts[&installment_id] {
            return Err(AppError::user(format!(
                "Taksit {installment_id} için ödemeler taksit tutarını aşıyor."
            )));
        }
    }

    Ok(())
}

fn require_unique_positive_id(id: i64, ids: &mut HashSet<i64>, record_name: &str) -> AppResult<()> {
    if id <= 0 || id > MAX_SAFE_JS_INTEGER || !ids.insert(id) {
        return Err(AppError::user(format!(
            "Geçersiz veya yinelenen {record_name} numarası: {id}."
        )));
    }
    Ok(())
}

pub(crate) fn validate_iso_date(value: &str, label: &str) -> AppResult<()> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map(|_| ())
        .map_err(|_| AppError::user(format!("{label} geçersiz: {value}.")))
}

pub(crate) fn validate_profile(profile: &BusinessProfile) -> AppResult<()> {
    for (label, value, max) in [
        ("İşletme adı", profile.name.as_str(), 200),
        ("Adres", profile.address.as_str(), 1_000),
        ("Telefon", profile.phone.as_str(), 100),
        ("Web sitesi", profile.website.as_str(), 500),
        ("Alt bilgi", profile.footer_sub.as_str(), 200),
    ] {
        if value.chars().count() > max {
            return Err(AppError::user(format!("{label} çok uzun.")));
        }
    }

    if !profile.website.trim().is_empty() {
        let parsed = url::Url::parse(profile.website.trim())
            .map_err(|_| AppError::user("İşletme web sitesi geçersiz."))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(AppError::user(
                "İşletme web sitesi http veya https kullanmalıdır.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, thread};

    use tempfile::TempDir;

    use super::*;

    fn test_database() -> (TempDir, Database) {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::initialize(directory.path().join("pusula.sqlite3")).unwrap();
        (directory, database)
    }

    #[test]
    fn initializes_schema_with_integrity_guards() {
        let (_directory, database) = test_database();
        let status = database.status().unwrap();
        assert_eq!(status.schema_version, SCHEMA_VERSION);
        assert_eq!(status.journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(status.integrity_check, "ok");
        assert!(Uuid::parse_str(&status.database_id).is_ok());
        assert!(!status.onboarding_complete);
        assert_eq!(status.last_import, None);
        assert_eq!(status.counts, RecordCounts::default());
        assert_eq!(status.totals, FinancialTotals::default());

        let connection = database.connect().unwrap();
        let foreign_keys: i32 = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn rejects_newer_database_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("newer.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(connection);

        let error = Database::initialize(path).unwrap_err().to_string();
        assert!(error.contains("daha yeni"));
    }

    #[test]
    fn persists_identity_onboarding_and_import_summary() {
        let (_source_directory, source) = test_database();
        let source_connection = source.connect().unwrap();
        source_connection
            .execute(
                "INSERT INTO customers(id, name, registration_date) VALUES (7, 'Aktarım', '2026-07-14')",
                [],
            )
            .unwrap();
        source_connection
            .execute(
                "INSERT INTO sales(id, customer_id, date, total_kurus, description) VALUES (11, 7, '2026-07-14', 12345, '')",
                [],
            )
            .unwrap();
        drop(source_connection);
        let bundle = source.export_data().unwrap();

        let (target_directory, target) = test_database();
        let database_id = target.status().unwrap().database_id;
        let summary = target.import_data(bundle, false).unwrap();
        let status = target.status().unwrap();
        assert!(status.onboarding_complete);
        assert_eq!(status.last_import, Some(summary.clone()));
        assert_eq!(status.totals, summary.totals);

        let reopened =
            Database::initialize(target_directory.path().join("pusula.sqlite3")).unwrap();
        let reopened_status = reopened.status().unwrap();
        assert_eq!(reopened_status.database_id, database_id);
        assert!(reopened_status.onboarding_complete);
        assert_eq!(reopened_status.last_import, Some(summary));
    }

    #[test]
    fn empty_start_and_preexisting_data_initialize_onboarding_safely() {
        let (empty_directory, empty) = test_database();
        empty.acknowledge_empty_start().unwrap();
        let reopened_empty =
            Database::initialize(empty_directory.path().join("pusula.sqlite3")).unwrap();
        assert!(reopened_empty.status().unwrap().onboarding_complete);

        let (legacy_directory, legacy) = test_database();
        let connection = legacy.connect().unwrap();
        connection
            .execute(
                "INSERT INTO customers(id, name, registration_date) VALUES (9, 'Mevcut', '2026-07-14')",
                [],
            )
            .unwrap();
        connection
            .execute("DELETE FROM settings WHERE key = 'onboarding_complete'", [])
            .unwrap();
        drop(connection);

        let reopened_legacy =
            Database::initialize(legacy_directory.path().join("pusula.sqlite3")).unwrap();
        assert!(reopened_legacy.status().unwrap().onboarding_complete);
    }

    #[test]
    fn import_settings_roll_back_with_failed_database_write() {
        let (_source_directory, source) = test_database();
        let source_connection = source.connect().unwrap();
        source_connection
            .execute(
                "INSERT INTO customers(id, name, registration_date) VALUES (5, 'Kaynak', '2026-07-14')",
                [],
            )
            .unwrap();
        drop(source_connection);
        let bundle = source.export_data().unwrap();

        let (_target_directory, target) = test_database();
        let target_connection = target.connect().unwrap();
        target_connection
            .execute(
                "INSERT INTO customers(id, name, registration_date) VALUES (5, 'Çakışma', '2026-07-14')",
                [],
            )
            .unwrap();
        drop(target_connection);
        assert!(target.import_data(bundle, false).is_err());
        let status = target.status().unwrap();
        assert!(!status.onboarding_complete);
        assert_eq!(status.last_import, None);
    }

    #[test]
    fn rejects_exports_and_import_ids_outside_javascript_safe_range() {
        let (_source_directory, source) = test_database();
        let source_connection = source.connect().unwrap();
        source_connection
            .execute(
                "INSERT INTO customers(id, name, registration_date) VALUES (1, 'Sınır', '2026-07-14')",
                [],
            )
            .unwrap();
        source_connection
            .execute(
                "INSERT INTO sales(id, customer_id, date, total_kurus, description) VALUES (1, 1, '2026-07-14', 100, '')",
                [],
            )
            .unwrap();
        drop(source_connection);
        let mut bundle = source.export_data().unwrap();
        bundle.customers[0].id = MAX_SAFE_JS_INTEGER + 1;
        bundle.sales[0].customer_id = MAX_SAFE_JS_INTEGER + 1;
        bundle.manifest.sha256 = bundle_checksum(&bundle).unwrap();

        let (_target_directory, target) = test_database();
        assert!(target.import_data(bundle, false).is_err());

        let unsafe_connection = target.connect().unwrap();
        unsafe_connection
            .execute(
                "INSERT INTO customers(id, name, registration_date) VALUES (2, 'Eski', '2026-07-14')",
                [],
            )
            .unwrap();
        unsafe_connection
            .execute(
                "INSERT INTO sales(id, customer_id, date, total_kurus, description) VALUES (2, 2, '2026-07-14', ?, '')",
                [MAX_SAFE_JS_INTEGER + 1],
            )
            .unwrap();
        drop(unsafe_connection);
        assert!(target.status().is_err());
        assert!(target.export_data().is_err());
    }

    #[test]
    fn export_uses_one_snapshot_across_concurrent_commit() {
        let (_directory, database) = test_database();
        let writer_database = database.clone();
        let (start_writer, writer_started) = mpsc::channel();
        let (writer_done, wait_for_writer) = mpsc::channel();
        let writer = thread::spawn(move || {
            writer_started.recv().unwrap();
            let connection = writer_database.connect().unwrap();
            connection
                .execute(
                    "INSERT INTO customers(id, name, registration_date) VALUES (1, 'Yeni', '2026-07-14')",
                    [],
                )
                .unwrap();
            writer_done.send(()).unwrap();
        });

        let bundle = database
            .export_data_with_hook(|| {
                start_writer.send(()).unwrap();
                wait_for_writer.recv().unwrap();
            })
            .unwrap();
        writer.join().unwrap();

        assert!(bundle.customers.is_empty());
        assert_eq!(bundle.manifest.counts.customers, 0);
        assert_eq!(bundle.manifest.sha256, bundle_checksum(&bundle).unwrap());
        assert_eq!(database.status().unwrap().counts.customers, 1);
    }
}
