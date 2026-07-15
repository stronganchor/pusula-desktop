use pusula_desktop_lib::{db::bundle_checksum, Database, ExportBundle};
use serde_json::{json, Value};
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

fn test_database(name: &str) -> (TempDir, Database) {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::initialize(directory.path().join(format!("{name}.sqlite3"))).unwrap();
    (directory, database)
}

fn api(database: &Database, path: &str, method: &str, body: Value) -> Value {
    database
        .api_request(path, Some(method), Some(body))
        .unwrap()
}

fn add_customer(database: &Database, id: i64, name: &str) {
    api(
        database,
        "/customers",
        "POST",
        json!({
            "id": id,
            "name": name,
            "phone": "555 000 00 00",
            "registration_date": "2026-07-01",
            "contacts": [{ "name": "Yakın", "phone": "555 111 11 11" }],
        }),
    );
}

#[test]
fn sale_and_installments_are_atomic_idempotent_and_keep_rounding_residue() {
    let (_directory, database) = test_database("atomic-sale");
    add_customer(&database, 1, "Deneme Müşteri");

    let body = json!({
        "customer_id": 1,
        "date": "2026-07-10",
        "total": 100,
        "down_payment": 0,
        "description": "Yuvarlama testi",
        "request_key": "sale-fixed-key",
        "installments": [
            { "due_date": "2026-08-10", "amount": 33.33 },
            { "due_date": "2026-09-10", "amount": 33.33 },
            { "due_date": "2026-10-10", "amount": 33.33 }
        ]
    });
    let created = api(&database, "/sales", "POST", body.clone());
    assert_eq!(created["installment_ids"].as_array().unwrap().len(), 3);

    let replay = api(&database, "/sales", "POST", body);
    assert_eq!(created, replay);
    let sale_id = created["id"].as_i64().unwrap();
    let sale = api(&database, &format!("/sales/{sale_id}"), "GET", Value::Null);
    let amounts: Vec<f64> = sale["installments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["amount"].as_f64().unwrap())
        .collect();
    assert_eq!(amounts, vec![33.33, 33.33, 33.34]);
    assert_eq!(sale["installments_total"], json!(100.0));

    let invalid = json!({
        "customer_id": 1,
        "date": "2026-07-11",
        "total": 50,
        "down_payment": 0,
        "request_key": "must-roll-back",
        "installments": [
            { "due_date": "2026-08-11", "amount": 25 },
            { "due_date": "not-a-date", "amount": 25 }
        ]
    });
    assert!(database
        .api_request("/sales", Some("POST"), Some(invalid))
        .is_err());
    let sales = api(&database, "/sales", "GET", Value::Null);
    assert_eq!(sales.as_array().unwrap().len(), 1);
}

#[test]
fn payments_reports_expected_rows_and_cascades_stay_consistent() {
    let (_directory, database) = test_database("payments");
    add_customer(&database, 7, "Tahsilat Müşterisi");
    let sale = api(
        &database,
        "/sales",
        "POST",
        json!({
            "customer_id": 7,
            "date": "2026-07-10",
            "total": 120,
            "down_payment": 20,
            "request_key": "payment-sale",
            "installments": [
                { "due_date": "2026-07-12", "amount": 50 },
                { "due_date": "2026-08-12", "amount": 50 }
            ]
        }),
    );
    let installment_id = sale["installment_ids"][0].as_i64().unwrap();
    let payment = api(
        &database,
        &format!("/installments/{installment_id}/payments"),
        "POST",
        json!({
            "amount": 30,
            "payment_date": "2026-07-14",
            "request_key": "payment-30-20260714"
        }),
    );
    assert_eq!(payment["installment"]["paid_amount"], json!(30.0));
    assert_eq!(payment["installment"]["remaining_amount"], json!(20.0));

    let overpayment = database.api_request(
        &format!("/installments/{installment_id}/payments"),
        Some("POST"),
        Some(json!({
            "amount": 21,
            "payment_date": "2026-07-14",
            "request_key": "payment-overpayment"
        })),
    );
    assert!(overpayment
        .unwrap_err()
        .to_string()
        .contains("kalan borçtan"));

    let report = api(
        &database,
        "/daily-report?start=2026-07-01&end=2026-07-31",
        "GET",
        Value::Null,
    );
    let events = report.as_array().unwrap();
    assert!(events
        .iter()
        .any(|row| row["event_type"] == "down_payment" && row["amount"] == json!(20.0)));
    assert!(events
        .iter()
        .any(|row| { row["event_type"] == "installment_payment" && row["amount"] == json!(30.0) }));

    let expected = api(
        &database,
        "/expected-payments?start=2026-07-01&end=2026-08-31",
        "GET",
        Value::Null,
    );
    assert_eq!(expected.as_array().unwrap().len(), 2);
    assert!(expected.as_array().unwrap().iter().any(|row| {
        row["installment_id"] == installment_id && row["remaining_amount"] == json!(20.0)
    }));

    let payment_id = payment["payment"]["id"].as_i64().unwrap();
    api(
        &database,
        &format!("/installments/{installment_id}/payments/{payment_id}"),
        "DELETE",
        Value::Null,
    );
    let installment = api(
        &database,
        &format!("/installments/{installment_id}"),
        "GET",
        Value::Null,
    );
    assert_eq!(installment["paid_amount"], json!(0.0));

    api(&database, "/customers/7", "DELETE", Value::Null);
    let status = database.status().unwrap();
    assert_eq!(status.counts.customers, 0);
    assert_eq!(status.counts.contacts, 0);
    assert_eq!(status.counts.sales, 0);
    assert_eq!(status.counts.installments, 0);
    assert_eq!(status.counts.payments, 0);
}

#[test]
fn export_import_and_file_round_trip_validate_before_writing() {
    let (source_directory, source) = test_database("source");
    add_customer(&source, 3, "Aktarım Müşterisi");
    api(
        &source,
        "/business-profile",
        "PUT",
        json!({
            "name": "ENES BEKO",
            "address": "Adana",
            "phone": "0322",
            "website": "https://example.com",
            "footer_sub": "Alt bilgi"
        }),
    );
    let sale = api(
        &source,
        "/sales",
        "POST",
        json!({
            "customer_id": 3,
            "date": "2026-07-01",
            "total": 75.25,
            "request_key": "export-sale",
            "installments": [
                { "due_date": "2026-08-01", "amount": 75.25 }
            ]
        }),
    );
    let installment_id = sale["installment_ids"][0].as_i64().unwrap();
    api(
        &source,
        &format!("/installments/{installment_id}/payments"),
        "POST",
        json!({
            "amount": 10,
            "payment_date": "2026-07-02",
            "request_key": "export-payment"
        }),
    );

    let bundle = source.export_data().unwrap();
    assert_eq!(bundle.manifest.counts.customers, 1);
    assert_eq!(bundle.manifest.totals.sales_kurus, 7_525);
    assert_eq!(
        bundle.payments[0].request_key.as_deref(),
        Some("export-payment")
    );
    assert_eq!(bundle.manifest.sha256, bundle_checksum(&bundle).unwrap());

    let mut normalized_payment_key = bundle.clone();
    normalized_payment_key.payments[0].request_key = Some("  export-payment!?  ".to_owned());
    normalized_payment_key.manifest.sha256 = bundle_checksum(&normalized_payment_key).unwrap();
    let (_normalized_directory, normalized_target) = test_database("normalized-payment-key");
    normalized_target
        .import_data(normalized_payment_key, false)
        .unwrap();
    assert_eq!(
        normalized_target.export_data().unwrap().payments[0]
            .request_key
            .as_deref(),
        Some("export-payment")
    );

    let mut duplicate_payment_keys = bundle.clone();
    duplicate_payment_keys.payments[0].request_key = Some("duplicate-key!".to_owned());
    let mut second_payment = duplicate_payment_keys.payments[0].clone();
    second_payment.id += 1;
    second_payment.request_key = Some("duplicate-key?".to_owned());
    duplicate_payment_keys.payments.push(second_payment);
    duplicate_payment_keys.manifest.counts.payments += 1;
    duplicate_payment_keys.manifest.totals.payments_kurus += 1_000;
    duplicate_payment_keys.manifest.sha256 = bundle_checksum(&duplicate_payment_keys).unwrap();
    let (_duplicate_directory, duplicate_target) = test_database("duplicate-payment-key");
    assert!(duplicate_target
        .import_data(duplicate_payment_keys, false)
        .unwrap_err()
        .to_string()
        .contains("ödeme istek anahtarı"));

    let (_target_directory, target) = test_database("target");
    let mut broken = bundle.clone();
    broken.customers[0].name = "Bozuk".to_owned();
    assert!(target.import_data(broken, false).is_err());
    assert_eq!(target.status().unwrap().counts.customers, 0);

    let summary = target.import_data(bundle.clone(), false).unwrap();
    assert!(!summary.replaced);
    assert_eq!(target.status().unwrap().counts.customers, 1);
    assert_eq!(target.export_data().unwrap().sales, bundle.sales);
    assert_eq!(target.export_data().unwrap().payments, bundle.payments);

    let export_path = source_directory.path().join("pusula-export.json");
    let file_summary = source.export_data_file(&export_path, false).unwrap();
    assert!(file_summary.bytes_written > 0);
    assert!(source.export_data_file(&export_path, false).is_err());

    let (_file_target_directory, file_target) = test_database("file-target");
    file_target.import_data_file(&export_path, false).unwrap();
    assert_eq!(file_target.status().unwrap().counts.customers, 1);

    let mut invalid_relation = bundle;
    invalid_relation.contacts[0].customer_id = 999;
    invalid_relation.manifest.sha256 = bundle_checksum(&invalid_relation).unwrap();
    let (_invalid_directory, invalid_target) = test_database("invalid-target");
    assert!(invalid_target
        .import_data(invalid_relation, false)
        .unwrap_err()
        .to_string()
        .contains("bilinmeyen müşteriye"));
    assert_eq!(invalid_target.status().unwrap().counts.customers, 0);
}

#[test]
fn omitted_replace_never_applies_inside_database_api() {
    // Database::import_data always requires an explicit bool; command-level
    // omission defaults to false. This test locks merge behavior itself.
    let (_directory, database) = test_database("merge");
    add_customer(&database, 1, "Mevcut");
    let bundle = database.export_data().unwrap();
    assert!(database.import_data(bundle, false).is_err());
    assert_eq!(database.status().unwrap().counts.customers, 1);
}

#[test]
fn imports_php_generated_wordpress_fixture_exactly() {
    let fixture: ExportBundle =
        serde_json::from_str(include_str!("../../tests/fixtures/pusula-lite-v1.json")).unwrap();
    assert_eq!(fixture.manifest.sha256, bundle_checksum(&fixture).unwrap());

    let (_directory, database) = test_database("wordpress-fixture");
    let summary = database.import_data(fixture, false).unwrap();
    assert_eq!(summary.counts.customers, 2);
    assert_eq!(summary.counts.contacts, 1);
    assert_eq!(summary.counts.sales, 2);
    assert_eq!(summary.counts.installments, 1);
    assert_eq!(summary.counts.payments, 1);
    assert_eq!(summary.totals.sales_kurus, 1_234_567_900);

    let exported = database.export_data().unwrap();
    assert_eq!(exported.customers[0].id, 2);
    assert_eq!(exported.customers[1].id, 7);
    assert_eq!(exported.sales[0].total_kurus, 1_234_567_890);
    assert_eq!(exported.installments[0].due_date, None);
    assert_eq!(exported.sales[0].request_key, None);
    assert_eq!(exported.payments[0].request_key, None);
    assert_eq!(
        exported.business_profile.website,
        "https://example.com/pusula"
    );
}

#[test]
fn verified_import_can_change_normally_and_reopen() {
    let fixture: ExportBundle =
        serde_json::from_str(include_str!("../../tests/fixtures/pusula-lite-v1.json")).unwrap();
    let (directory, database) = test_database("verified-import-lifecycle");
    let summary = database.import_data(fixture, false).unwrap();
    assert!(database.status().unwrap().import_verification_pending);
    database.acknowledge_import_verification(&summary).unwrap();
    assert!(!database.status().unwrap().import_verification_pending);

    add_customer(&database, 99, "İçe Aktarma Sonrası Müşteri");
    let sale = api(
        &database,
        "/sales",
        "POST",
        json!({
            "customer_id": 99,
            "date": "2026-07-15",
            "total": 50,
            "request_key": "post-import-sale",
            "installments": [{ "due_date": "2026-08-15", "amount": 50 }]
        }),
    );
    let installment_id = sale["installment_ids"][0].as_i64().unwrap();
    api(
        &database,
        &format!("/installments/{installment_id}/payments"),
        "POST",
        json!({
            "amount": 10,
            "payment_date": "2026-07-15",
            "request_key": "post-import-payment"
        }),
    );
    drop(database);

    let reopened =
        Database::initialize(directory.path().join("verified-import-lifecycle.sqlite3")).unwrap();
    let reopened_status = reopened.status().unwrap();
    assert!(!reopened_status.import_verification_pending);
    assert_ne!(reopened_status.counts, summary.counts);
    assert_ne!(reopened_status.totals, summary.totals);
    assert_eq!(reopened_status.last_import, Some(summary));
}

#[test]
fn payment_request_keys_replay_once_and_reject_conflicting_reuse() {
    let (_directory, database) = test_database("payment-idempotency");
    add_customer(&database, 88, "Tekrarsız Tahsilat");
    let sale = api(
        &database,
        "/sales",
        "POST",
        json!({
            "customer_id": 88,
            "date": "2026-07-15",
            "total": 100,
            "request_key": "payment-idempotency-sale",
            "installments": [
                { "due_date": "2026-08-15", "amount": 60 },
                { "due_date": "2026-09-15", "amount": 40 }
            ]
        }),
    );
    let first_installment = sale["installment_ids"][0].as_i64().unwrap();
    let second_installment = sale["installment_ids"][1].as_i64().unwrap();
    let payment_body = json!({
        "amount": 25,
        "payment_date": "2026-07-15",
        "request_key": "stable-payment-request"
    });

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let database = database.clone();
        let barrier = barrier.clone();
        let body = payment_body.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            api(
                &database,
                &format!("/installments/{first_installment}/payments"),
                "POST",
                body,
            )
        }));
    }
    let first = handles.remove(0).join().unwrap();
    let replay = handles.remove(0).join().unwrap();
    assert_eq!(first["payment"]["id"], replay["payment"]["id"]);
    let listed = api(
        &database,
        &format!("/installments/{first_installment}/payments"),
        "GET",
        Value::Null,
    );
    assert_eq!(listed["payments"].as_array().unwrap().len(), 1);
    assert_eq!(listed["installment"]["paid_amount"], json!(25.0));

    for (installment_id, amount, date) in [
        (second_installment, 25, "2026-07-15"),
        (first_installment, 20, "2026-07-15"),
        (first_installment, 25, "2026-07-16"),
    ] {
        let error = database
            .api_request(
                &format!("/installments/{installment_id}/payments"),
                Some("POST"),
                Some(json!({
                    "amount": amount,
                    "payment_date": date,
                    "request_key": "stable-payment-request"
                })),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("farklı bir tahsilat"));
    }
    assert_eq!(database.status().unwrap().counts.payments, 1);
}
