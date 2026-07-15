use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{header, HeaderMap, Request, Response, StatusCode},
    routing::put,
    Router,
};
use chrono::Utc;
use pusula_backup_gateway::{
    api::{cleanup_stale_relay_spools, router, GatewayState},
    b2::B2Client,
    config::B2Config,
    db::Database,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

const CHECKSUM: &str = "abababababababababababababababababababababababababababababababab";
const RELAY_BODY: &[u8] = b"age-encrypted-ciphertext";
const RELAY_CHECKSUM: &str = "6b597aacb68032c8695418c61b29d0b50ad7e37e273eb8bfc421d751baca196f";
const GOOD_CHECKSUM: &str = "770e607624d689265ca6c44884d0807d9b054d23c473c106c72be9de08b7376c";

struct TestGateway {
    directory: TempDir,
    database: Database,
    router: Router,
    mock_server: tokio::task::JoinHandle<()>,
    storage_puts: Arc<AtomicUsize>,
    storage_bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.mock_server.abort();
    }
}

async fn gateway(object_size: u64, object_checksum: &'static str) -> TestGateway {
    gateway_with_put_status(object_size, object_checksum, StatusCode::OK).await
}

async fn gateway_with_put_status(
    object_size: u64,
    object_checksum: &'static str,
    put_status: StatusCode,
) -> TestGateway {
    let storage_puts = Arc::new(AtomicUsize::new(0));
    let storage_bodies = Arc::new(Mutex::new(Vec::new()));
    let put_counter = storage_puts.clone();
    let put_bodies = storage_bodies.clone();
    let storage = Router::new().route(
        "/{*object}",
        put(move |headers: HeaderMap, body: Body| {
            let counter = put_counter.clone();
            let bodies = put_bodies.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let bytes = to_bytes(body, 2048).await.unwrap();
                bodies.lock().unwrap().push(bytes.to_vec());
                if headers
                    .get(header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    != Some(object_size.to_string().as_str())
                    || headers
                        .get("x-amz-meta-sha256")
                        .and_then(|value| value.to_str().ok())
                        != Some(object_checksum)
                    || headers
                        .get("x-amz-server-side-encryption")
                        .and_then(|value| value.to_str().ok())
                        != Some("AES256")
                {
                    return StatusCode::BAD_REQUEST;
                }
                put_status
            }
        })
        .head(move || async move {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                object_size.to_string().parse().unwrap(),
            );
            response
                .headers_mut()
                .insert("x-amz-meta-sha256", object_checksum.parse().unwrap());
            response
                .headers_mut()
                .insert("x-amz-server-side-encryption", "AES256".parse().unwrap());
            response
                .headers_mut()
                .insert(header::ETAG, "test-etag".parse().unwrap());
            response
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mock_server = tokio::spawn(async move {
        axum::serve(listener, storage).await.unwrap();
    });

    let directory = TempDir::new().unwrap();
    let database = Database::new(
        directory.path().join("gateway.sqlite3"),
        Arc::from(b"integration-test-pepper-at-least-32-bytes".as_slice()),
    );
    database.migrate().unwrap();
    let b2 = B2Client::new(
        B2Config {
            endpoint: Url::parse(&format!("http://{address}")).unwrap(),
            region: "test-region".to_owned(),
            bucket: "test-private-bucket".to_owned(),
            prefix: "backups/".to_owned(),
            key_id: "test-key-id".to_owned(),
            application_key: "test-application-key".to_owned(),
            presign_ttl: Duration::from_secs(900),
        },
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    );
    let router = router(GatewayState::new(
        database.clone(),
        b2,
        "backups/",
        1024,
        5,
        Duration::from_secs(60),
    ));
    TestGateway {
        directory,
        database,
        router,
        mock_server,
        storage_puts,
        storage_bodies,
    }
}

async fn json_request(
    router: &Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder
        .body(match body {
            Some(value) => Body::from(serde_json::to_vec(&value).unwrap()),
            None => Body::empty(),
        })
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

async fn relay_request(
    router: &Router,
    backup_id: &str,
    bearer: &str,
    declared_length: usize,
    body: &[u8],
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/v1/backups/relay/{backup_id}"))
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, declared_length.to_string())
        .body(Body::from(body.to_vec()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

fn checksum(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn relay_spool_is_empty(gateway: &TestGateway) -> bool {
    let directory = gateway.directory.path().join("relay-spool");
    !directory.exists() || directory.read_dir().unwrap().next().is_none()
}

#[tokio::test]
async fn enroll_upload_complete_and_status_round_trip() {
    let gateway = gateway(100, CHECKSUM).await;
    let enrollment = gateway
        .database
        .issue_enrollment("initial installation", 300)
        .unwrap();
    let (status, enrolled) = json_request(
        &gateway.router,
        "POST",
        "/v1/enroll",
        None,
        Some(json!({
            "enrollment_code": enrollment.enrollment_code,
            "device_name": "Front Desk"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let token = enrolled["device_token"].as_str().unwrap();
    assert!(token.starts_with("pdt_"));

    let (status, _) = json_request(
        &gateway.router,
        "POST",
        "/v1/enroll",
        None,
        Some(json!({
            "enrollment_code": enrollment.enrollment_code,
            "device_name": "Replay"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(token),
        Some(json!({ "content_length": 100, "sha256": CHECKSUM })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{upload}");
    assert_eq!(upload["method"], "PUT");
    assert_eq!(upload["retention_class"], "rolling");
    assert_eq!(upload["required_headers"]["content-length"], "100");
    assert_eq!(
        upload["required_headers"]["x-amz-server-side-encryption"],
        "AES256"
    );
    assert!(upload["upload_url"]
        .as_str()
        .unwrap()
        .contains(address_fragment(&gateway)));
    assert!(upload["upload_url"]
        .as_str()
        .unwrap()
        .contains("/test-private-bucket/backups/rolling/"));
    let backup_id = upload["backup_id"].as_str().unwrap();

    let (status, completed) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/complete",
        Some(token),
        Some(json!({ "backup_id": backup_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["etag"], "test-etag");

    let (status, status_body) = json_request(
        &gateway.router,
        "GET",
        "/v1/backups/status",
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_body["latest_completed"]["backup_id"], backup_id);
    assert_eq!(status_body["active_pending_uploads"], 0);
}

#[tokio::test]
async fn relay_upload_is_authenticated_verified_cleaned_and_idempotent() {
    assert_eq!(checksum(RELAY_BODY), RELAY_CHECKSUM);
    let gateway = gateway(RELAY_BODY.len() as u64, RELAY_CHECKSUM).await;
    let device = gateway.database.issue_device("relay test", 5).unwrap();
    let (status, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({
            "content_length": RELAY_BODY.len(),
            "sha256": RELAY_CHECKSUM
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{upload}");
    let backup_id = upload["backup_id"].as_str().unwrap();

    let (status, completed) = relay_request(
        &gateway.router,
        backup_id,
        &device.device_token,
        RELAY_BODY.len(),
        RELAY_BODY,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["backup_id"], backup_id);
    assert_eq!(completed["status"], "completed");
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 1);
    assert_eq!(
        gateway.storage_bodies.lock().unwrap().as_slice(),
        &[RELAY_BODY.to_vec()]
    );
    assert!(relay_spool_is_empty(&gateway));

    let (status, duplicate) = relay_request(
        &gateway.router,
        backup_id,
        &device.device_token,
        RELAY_BODY.len(),
        RELAY_BODY,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{duplicate}");
    assert_eq!(duplicate["status"], "completed");
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 1);
    assert!(relay_spool_is_empty(&gateway));
}

#[tokio::test]
async fn relay_allows_only_one_in_flight_ciphertext_body() {
    let gateway = gateway(RELAY_BODY.len() as u64, RELAY_CHECKSUM).await;
    let device = gateway
        .database
        .issue_device("relay concurrency", 5)
        .unwrap();
    let (_, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({
            "content_length": RELAY_BODY.len(),
            "sha256": RELAY_CHECKSUM
        })),
    )
    .await;
    let backup_id = upload["backup_id"].as_str().unwrap().to_owned();

    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let stream = futures_util::stream::once(async move {
        let _ = started_tx.send(());
        let _ = release_rx.await;
        Ok::<Bytes, Infallible>(Bytes::from_static(RELAY_BODY))
    });
    let first_request = Request::builder()
        .method("PUT")
        .uri(format!("/v1/backups/relay/{backup_id}"))
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", device.device_token),
        )
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, RELAY_BODY.len().to_string())
        .body(Body::from_stream(stream))
        .unwrap();
    let first_router = gateway.router.clone();
    let first = tokio::spawn(async move { first_router.oneshot(first_request).await.unwrap() });
    started_rx.await.unwrap();

    let (status, _) = relay_request(
        &gateway.router,
        &backup_id,
        &device.device_token,
        RELAY_BODY.len(),
        RELAY_BODY,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    release_tx.send(()).unwrap();
    let first_response = first.await.unwrap();
    assert_eq!(first_response.status(), StatusCode::OK);
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 1);
    assert!(relay_spool_is_empty(&gateway));
}

#[tokio::test]
async fn relay_rejects_auth_oversize_and_checksum_mismatch_before_b2() {
    let gateway = gateway(4, GOOD_CHECKSUM).await;
    let device = gateway.database.issue_device("relay test", 5).unwrap();
    let (_, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({ "content_length": 4, "sha256": GOOD_CHECKSUM })),
    )
    .await;
    let backup_id = upload["backup_id"].as_str().unwrap();

    let request = Request::builder()
        .method("PUT")
        .uri(format!("/v1/backups/relay/{backup_id}"))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, "4")
        .body(Body::from("good"))
        .unwrap();
    let response = gateway.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let (status, _) = relay_request(
        &gateway.router,
        backup_id,
        &device.device_token,
        4,
        b"good!",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(relay_spool_is_empty(&gateway));

    let (status, _) =
        relay_request(&gateway.router, backup_id, &device.device_token, 4, b"evil").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 0);
    assert!(relay_spool_is_empty(&gateway));
}

#[tokio::test]
async fn relay_b2_failure_stays_pending_and_removes_spool() {
    let gateway = gateway_with_put_status(
        RELAY_BODY.len() as u64,
        RELAY_CHECKSUM,
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    let device = gateway.database.issue_device("relay test", 5).unwrap();
    let (_, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({
            "content_length": RELAY_BODY.len(),
            "sha256": RELAY_CHECKSUM
        })),
    )
    .await;
    let backup_id = upload["backup_id"].as_str().unwrap();

    let (status, _) = relay_request(
        &gateway.router,
        backup_id,
        &device.device_token,
        RELAY_BODY.len(),
        RELAY_BODY,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    for _ in 0..4 {
        let (status, _) = relay_request(
            &gateway.router,
            backup_id,
            &device.device_token,
            RELAY_BODY.len(),
            RELAY_BODY,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
    let (status, _) = relay_request(
        &gateway.router,
        backup_id,
        &device.device_token,
        RELAY_BODY.len(),
        RELAY_BODY,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 5);
    assert!(relay_spool_is_empty(&gateway));

    let (status, status_body) = json_request(
        &gateway.router,
        "GET",
        "/v1/backups/status",
        Some(&device.device_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(status_body["latest_completed"].is_null());
    assert_eq!(status_body["active_pending_uploads"], 1);
}

#[tokio::test]
async fn relay_can_resume_an_expired_pending_presigned_reservation() {
    let gateway = gateway(RELAY_BODY.len() as u64, RELAY_CHECKSUM).await;
    let device = gateway.database.issue_device("expired relay", 5).unwrap();
    let backup_id = Uuid::new_v4().to_string();
    gateway
        .database
        .reserve_backup(
            &device.device_id,
            &backup_id,
            &format!(
                "backups/rolling/{}/2026/07/15/{backup_id}.sqlite3.age",
                device.device_id
            ),
            RELAY_BODY.len() as u64,
            RELAY_CHECKSUM,
            Utc::now().timestamp() + 900,
            5,
            60,
        )
        .unwrap();
    let now = Utc::now().timestamp();
    rusqlite::Connection::open(gateway.database.path())
        .unwrap()
        .execute(
            "UPDATE backups SET created_at = ?2, upload_expires_at = ?3 WHERE id = ?1",
            rusqlite::params![backup_id, now - 120, now - 60],
        )
        .unwrap();

    let (status, completed) = relay_request(
        &gateway.router,
        &backup_id,
        &device.device_token,
        RELAY_BODY.len(),
        RELAY_BODY,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["status"], "completed");
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 1);
    assert!(relay_spool_is_empty(&gateway));
}

#[tokio::test]
async fn startup_cleanup_removes_only_stale_relay_parts() {
    let directory = TempDir::new().unwrap();
    let database_path = directory.path().join("gateway.sqlite3");
    let relay_directory = directory.path().join("relay-spool");
    std::fs::create_dir(&relay_directory).unwrap();
    let stale =
        relay_directory.join(".relay-test-00000000-0000-4000-8000-000000000001.sqlite3.age.part");
    let unrelated = relay_directory.join("operator-note.keep");
    std::fs::write(&stale, b"encrypted fragment").unwrap();
    std::fs::write(&unrelated, b"keep").unwrap();

    let removed = cleanup_stale_relay_spools(&database_path).await.unwrap();
    assert_eq!(removed, 1);
    assert!(!stale.exists());
    assert!(unrelated.exists());
}

fn address_fragment(_gateway: &TestGateway) -> &'static str {
    // The exact ephemeral port is intentionally opaque to the fixture; a local
    // HTTP URL proves that the presigner retained the configured endpoint.
    "http://127.0.0.1:"
}

#[tokio::test]
async fn authentication_size_and_storage_verification_fail_closed() {
    let gateway = gateway(99, CHECKSUM).await;
    let device = gateway.database.issue_device("test", 5).unwrap();

    let (status, _) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        None,
        Some(json!({ "content_length": 100, "sha256": CHECKSUM })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({ "content_length": 1025, "sha256": CHECKSUM })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({ "content_length": 100, "sha256": CHECKSUM })),
    )
    .await;
    let backup_id = upload["backup_id"].as_str().unwrap();
    let (status, _) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/complete",
        Some(&device.device_token),
        Some(json!({ "backup_id": backup_id })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    let (_, status_body) = json_request(
        &gateway.router,
        "GET",
        "/v1/backups/status",
        Some(&device.device_token),
        None,
    )
    .await;
    assert!(status_body["latest_completed"].is_null());
    assert_eq!(status_body["active_pending_uploads"], 1);
}

#[tokio::test]
async fn health_is_minimal_process_liveness() {
    let gateway = gateway(1, CHECKSUM).await;
    let request = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let response = gateway.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "no-store, max-age=0"
    );
}
