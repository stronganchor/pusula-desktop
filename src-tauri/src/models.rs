use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
pub struct BusinessProfile {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub website: String,
    #[serde(default, alias = "footerSub")]
    pub footer_sub: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct CustomerExport {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub work_address: String,
    #[serde(default)]
    pub notes: String,
    pub registration_date: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct ContactExport {
    pub id: i64,
    pub customer_id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub home_address: String,
    #[serde(default)]
    pub work_address: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct SaleExport {
    pub id: i64,
    pub customer_id: i64,
    pub date: String,
    pub total_kurus: i64,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub request_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct InstallmentExport {
    pub id: i64,
    pub sale_id: i64,
    #[serde(default)]
    pub due_date: Option<String>,
    pub amount_kurus: i64,
    #[serde(default)]
    pub paid_date: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct PaymentExport {
    pub id: i64,
    pub installment_id: i64,
    pub amount_kurus: i64,
    pub payment_date: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
pub struct RecordCounts {
    pub customers: usize,
    pub contacts: usize,
    pub sales: usize,
    pub installments: usize,
    pub payments: usize,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
pub struct FinancialTotals {
    pub sales_kurus: i64,
    pub installments_kurus: i64,
    pub payments_kurus: i64,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
pub struct ExportManifest {
    pub counts: RecordCounts,
    pub totals: FinancialTotals,
    #[serde(default)]
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct ExportBundle {
    pub format_version: u32,
    pub source: String,
    pub source_version: String,
    pub exported_at: String,
    #[serde(alias = "business")]
    pub business_profile: BusinessProfile,
    #[serde(default)]
    pub customers: Vec<CustomerExport>,
    #[serde(default)]
    pub contacts: Vec<ContactExport>,
    #[serde(default)]
    pub sales: Vec<SaleExport>,
    #[serde(default)]
    pub installments: Vec<InstallmentExport>,
    #[serde(default)]
    pub payments: Vec<PaymentExport>,
    pub manifest: ExportManifest,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct ImportSummary {
    pub replaced: bool,
    pub counts: RecordCounts,
    pub totals: FinancialTotals,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct ExportSummary {
    pub path: String,
    pub bytes_written: u64,
    pub counts: RecordCounts,
    pub totals: FinancialTotals,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub struct DatabaseStatus {
    pub path: String,
    pub database_id: String,
    pub schema_version: i32,
    pub journal_mode: String,
    pub integrity_check: String,
    pub last_modified_at: Option<String>,
    pub onboarding_complete: bool,
    pub import_verification_pending: bool,
    pub last_import: Option<ImportSummary>,
    pub counts: RecordCounts,
    pub totals: FinancialTotals,
}
