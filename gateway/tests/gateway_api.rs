use std::{path::Path, sync::Arc, time::Duration};

use axum::{
    body::{to_bytes, Body},
    http::{header, Method, Request, StatusCode},
    Router,
};
use pusula_backup_gateway::{
    api::{self, cleanup_stale_relay_spools, GatewayLimits, GatewayState},
    db::{AdmissionPolicy, Database, RetentionPolicy},
    storage::LocalObjectStore,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;

struct TestGateway {
    _directory: TempDir,
    database: Database,
    storage: LocalObjectStore,
    app: Router,
}

impl TestGateway {
    async fn enroll(&self) -> (String, String) {
        let credential = self.database.issue_enrollment("test device", 3600).unwrap();
        let response = json_request(
            &self.app,
            Method::POST,
            "/v1/enroll",
            None,
            json!({
                "enrollment_code": credential.enrollment_code,
                "device_name": "Front Desk"
            }),
        )
        .await;
        assert_eq!(response.0, StatusCode::CREATED);
        let body = response.1;
        (
            body["device_id"].as_str().unwrap().to_owned(),
            body["device_token"].as_str().unwrap().to_owned(),
        )
    }

    async fn reserve(&self, token: &str, body: &[u8], retention: &str) -> Value {
        let response = json_request(
            &self.app,
            Method::POST,
            "/v1/backups/upload-url",
            Some(token),
            json!({
                "content_length": body.len(),
                "sha256": checksum(body),
                "retention_class": retention
            }),
        )
        .await;
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        response.1
    }
}

async fn gateway() -> TestGateway {
    gateway_with_limits(test_limits()).await
}

async fn gateway_with_limits(limits: GatewayLimits) -> TestGateway {
    let directory = TempDir::new().unwrap();
    let database = Database::new(
        directory.path().join("gateway.sqlite3"),
        Arc::from(b"test-only-pepper-at-least-32-bytes".as_slice()),
    );
    database.migrate().unwrap();
    let storage = LocalObjectStore::new(directory.path().join("objects"), 0).unwrap();
    let state = GatewayState::new(database.clone(), storage.clone(), "backups/", limits).unwrap();
    TestGateway {
        _directory: directory,
        database,
        storage,
        app: api::router(state),
    }
}

fn test_limits() -> GatewayLimits {
    GatewayLimits {
        max_backup_bytes: 1024 * 1024,
        reservation_ttl: Duration::from_secs(900),
        admission_policy: AdmissionPolicy {
            rate_capacity: 10,
            rate_refill_seconds: 1,
            max_pending_per_device: 8,
            byte_quota_24h: 20 * 1024 * 1024,
            pending_max_age_seconds: 30 * 24 * 60 * 60,
            pending_cleanup_limit: 100,
            authorization_cleanup_limit: 500,
            daily_min_interval_seconds: 20 * 60 * 60,
            monthly_min_interval_seconds: 25 * 24 * 60 * 60,
        },
        retention_policy: RetentionPolicy {
            rolling_seconds: 14 * 24 * 60 * 60,
            daily_seconds: 60 * 24 * 60 * 60,
            monthly_seconds: 400 * 24 * 60 * 60,
            pending_max_age_seconds: 30 * 24 * 60 * 60,
            pending_cleanup_limit: 100,
            cleanup_limit: 100,
        },
        max_request_concurrency: 8,
        max_db_concurrency: 4,
        global_request_capacity: 100,
        global_request_refill: Duration::from_secs(1),
    }
}

async fn json_request(
    app: &Router,
    method: Method,
    path: &str,
    token: Option<&str>,
    value: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(value.to_string())).unwrap())
        .await
        .unwrap();
    response_json(response).await
}

async fn relay_request(
    app: &Router,
    token: Option<&str>,
    backup_id: &str,
    body: &[u8],
    declared_length: Option<usize>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(Method::PUT)
        .uri(format!("/v1/backups/relay/{backup_id}"))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_LENGTH,
            declared_length.unwrap_or(body.len()).to_string(),
        );
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap();
    response_json(response).await
}

async fn response_json(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

fn checksum(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn file_count(path: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                file_count(&entry.path())
            } else {
                usize::from(entry.file_type().is_ok_and(|kind| kind.is_file()))
            }
        })
        .sum()
}

fn relay_spool_is_empty(gateway: &TestGateway) -> bool {
    let spool = gateway
        .database
        .path()
        .parent()
        .unwrap()
        .join("relay-spool");
    std::fs::read_dir(spool)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true)
}

#[tokio::test]
async fn enroll_reserve_relay_complete_status_and_download_round_trip() {
    let gateway = gateway().await;
    let (device_id, token) = gateway.enroll().await;
    let body = b"age-encrypted-sqlite-ciphertext";
    let hash = checksum(body);
    let reservation = gateway.reserve(&token, body, "rolling").await;
    let response_keys = reservation.as_object().unwrap();
    assert_eq!(response_keys.len(), 4);
    assert_eq!(reservation["transport"], "gateway_relay");
    assert_eq!(reservation["retention_class"], "rolling");
    assert!(reservation["expires_at"].as_str().unwrap().ends_with('Z'));
    let backup_id = reservation["backup_id"].as_str().unwrap();

    let absent = json_request(
        &gateway.app,
        Method::POST,
        "/v1/backups/complete",
        Some(&token),
        json!({ "backup_id": backup_id }),
    )
    .await;
    assert_eq!(absent.0, StatusCode::CONFLICT);
    assert_eq!(absent.1["error"]["code"], "object_not_present");

    let relayed = relay_request(&gateway.app, Some(&token), backup_id, body, None).await;
    assert_eq!(relayed.0, StatusCode::OK, "{}", relayed.1);
    assert_eq!(relayed.1["status"], "completed");
    assert_eq!(relayed.1["version_id"], format!("fs-sha256-{hash}"));
    assert!(relay_spool_is_empty(&gateway));
    assert_eq!(file_count(gateway.storage.root()), 1);

    let record = gateway.database.completed_backup(backup_id).unwrap();
    assert_eq!(record.device_id, device_id);
    assert_eq!(record.sha256, hash);
    assert_eq!(record.verified_sha256.as_deref(), Some(hash.as_str()));
    assert_eq!(record.verified_size_bytes, Some(body.len() as u64));
    let stored = tokio::fs::read(gateway.storage.root().join(&record.object_key))
        .await
        .unwrap();
    assert_eq!(stored, body);

    let status = json_request(
        &gateway.app,
        Method::GET,
        "/v1/backups/status",
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status.0, StatusCode::OK);
    assert_eq!(status.1["latest_completed"]["backup_id"], backup_id);
    assert_eq!(status.1["active_pending_uploads"], 0);

    let output = gateway
        .storage
        .root()
        .parent()
        .unwrap()
        .join("download.age");
    gateway
        .storage
        .download_verified(
            &record.object_key,
            record.version_id.as_deref().unwrap(),
            record.size_bytes,
            &record.sha256,
            &output,
        )
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(output).await.unwrap(), body);
}

#[tokio::test]
async fn lost_relay_response_retry_is_idempotent_and_creates_one_object() {
    let gateway = gateway().await;
    let (_, token) = gateway.enroll().await;
    let body = b"same-ciphertext-on-retry";
    let reservation = gateway.reserve(&token, body, "rolling").await;
    let backup_id = reservation["backup_id"].as_str().unwrap();

    let first = relay_request(&gateway.app, Some(&token), backup_id, body, None).await;
    assert_eq!(first.0, StatusCode::OK);
    let second = relay_request(&gateway.app, Some(&token), backup_id, body, None).await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(first.1["version_id"], second.1["version_id"]);
    assert_eq!(file_count(gateway.storage.root()), 1);
    assert!(relay_spool_is_empty(&gateway));
}

#[tokio::test]
async fn completed_acknowledgements_rehash_the_immutable_object() {
    let gateway = gateway().await;
    let (_, token) = gateway.enroll().await;
    let body = b"completed-ciphertext";
    let reservation = gateway.reserve(&token, body, "rolling").await;
    let backup_id = reservation["backup_id"].as_str().unwrap();
    let relayed = relay_request(&gateway.app, Some(&token), backup_id, body, None).await;
    assert_eq!(relayed.0, StatusCode::OK);

    let record = gateway.database.completed_backup(backup_id).unwrap();
    tokio::fs::write(
        gateway.storage.root().join(&record.object_key),
        vec![b'X'; body.len()],
    )
    .await
    .unwrap();

    let completion = json_request(
        &gateway.app,
        Method::POST,
        "/v1/backups/complete",
        Some(&token),
        json!({ "backup_id": backup_id }),
    )
    .await;
    assert_eq!(completion.0, StatusCode::BAD_GATEWAY);
    assert_eq!(completion.1["error"]["code"], "storage_verification_failed");

    let relay_retry = relay_request(&gateway.app, Some(&token), backup_id, body, None).await;
    assert_eq!(relay_retry.0, StatusCode::BAD_GATEWAY);
    assert_eq!(
        relay_retry.1["error"]["code"],
        "storage_verification_failed"
    );
}

#[tokio::test]
async fn relay_rejects_auth_length_and_checksum_failures_without_storing() {
    let gateway = gateway().await;
    let (_, token) = gateway.enroll().await;
    let body = b"expected-ciphertext";
    let reservation = gateway.reserve(&token, body, "rolling").await;
    let backup_id = reservation["backup_id"].as_str().unwrap();

    let no_auth = relay_request(&gateway.app, None, backup_id, body, None).await;
    assert_eq!(no_auth.0, StatusCode::UNAUTHORIZED);
    let wrong_length = relay_request(
        &gateway.app,
        Some(&token),
        backup_id,
        body,
        Some(body.len() + 1),
    )
    .await;
    assert_eq!(wrong_length.0, StatusCode::BAD_REQUEST);
    let wrong_body = relay_request(
        &gateway.app,
        Some(&token),
        backup_id,
        b"tampered-ciphertext",
        None,
    )
    .await;
    assert_eq!(wrong_body.0, StatusCode::BAD_REQUEST);
    assert_eq!(file_count(gateway.storage.root()), 0);
    assert!(relay_spool_is_empty(&gateway));
}

#[tokio::test]
async fn reservation_is_authenticated_bounded_and_reuses_identical_pending_ciphertext() {
    let mut limits = test_limits();
    limits.max_backup_bytes = 32;
    limits.admission_policy.byte_quota_24h = 128;
    let gateway = gateway_with_limits(limits).await;
    let (_, token) = gateway.enroll().await;
    let body = b"small-ciphertext";

    let unauthorized = json_request(
        &gateway.app,
        Method::POST,
        "/v1/backups/upload-url",
        None,
        json!({ "content_length": body.len(), "sha256": checksum(body) }),
    )
    .await;
    assert_eq!(unauthorized.0, StatusCode::UNAUTHORIZED);
    let oversized = json_request(
        &gateway.app,
        Method::POST,
        "/v1/backups/upload-url",
        Some(&token),
        json!({ "content_length": 33, "sha256": checksum(body) }),
    )
    .await;
    assert_eq!(oversized.0, StatusCode::BAD_REQUEST);

    let first = gateway.reserve(&token, body, "monthly").await;
    let second = gateway.reserve(&token, body, "monthly").await;
    assert_eq!(first["backup_id"], second["backup_id"]);
    assert_eq!(first["transport"], "gateway_relay");
}

#[tokio::test]
async fn startup_cleanup_removes_only_stale_relay_parts() {
    let directory = TempDir::new().unwrap();
    let database_path = directory.path().join("gateway.sqlite3");
    let spool = directory.path().join("relay-spool");
    std::fs::create_dir_all(&spool).unwrap();
    std::fs::write(spool.join(".relay-one.sqlite3.age.part"), b"stale").unwrap();
    std::fs::write(spool.join("keep.sqlite3.age"), b"keep").unwrap();
    std::fs::create_dir(spool.join(".relay-directory.sqlite3.age.part")).unwrap();

    assert_eq!(cleanup_stale_relay_spools(&database_path).await.unwrap(), 1);
    assert!(!spool.join(".relay-one.sqlite3.age.part").exists());
    assert!(spool.join("keep.sqlite3.age").exists());
    assert!(spool.join(".relay-directory.sqlite3.age.part").exists());
}

#[tokio::test]
async fn health_is_minimal_and_bypasses_authentication() {
    let gateway = gateway().await;
    let response = gateway
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "no-store, max-age=0"
    );
}
