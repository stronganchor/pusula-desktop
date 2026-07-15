use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{Path as RoutePath, RawQuery, State},
    http::{header, HeaderMap, Request, Response, StatusCode},
    routing::put,
    Router,
};
use pusula_backup_gateway::{
    api::{cleanup_stale_relay_spools, router, GatewayLimits, GatewayState},
    b2::B2Client,
    config::B2Config,
    db::{now_epoch, AdmissionPolicy, Database, RetentionClass},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

const DIRECT_BODY: &[u8] = b"good";
const RELAY_BODY: &[u8] = b"age-encrypted-ciphertext";
const RELAY_CHECKSUM: &str = "6b597aacb68032c8695418c61b29d0b50ad7e37e273eb8bfc421d751baca196f";
const GOOD_CHECKSUM: &str = "770e607624d689265ca6c44884d0807d9b054d23c473c106c72be9de08b7376c";

#[derive(Clone)]
struct StoredVersion {
    body: Vec<u8>,
    version_id: String,
    etag: String,
}

#[derive(Default)]
struct StoredObjects {
    versions: HashMap<String, Vec<StoredVersion>>,
    next_version: usize,
}

#[derive(Clone)]
struct MockStorage {
    objects: Arc<Mutex<StoredObjects>>,
    put_status: StatusCode,
    store_on_error: bool,
    puts: Arc<AtomicUsize>,
    gets: Arc<AtomicUsize>,
    fail_get_number: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

struct TestGateway {
    directory: TempDir,
    database: Database,
    router: Router,
    mock_server: tokio::task::JoinHandle<()>,
    storage_puts: Arc<AtomicUsize>,
    storage_gets: Arc<AtomicUsize>,
    fail_get_number: Arc<AtomicUsize>,
    storage_bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    b2: B2Client,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.mock_server.abort();
    }
}

async fn gateway() -> TestGateway {
    gateway_with_put_status(StatusCode::OK).await
}

async fn gateway_with_put_status(put_status: StatusCode) -> TestGateway {
    gateway_with_limits(put_status, test_limits()).await
}

fn test_limits() -> GatewayLimits {
    GatewayLimits {
        max_backup_bytes: 1024,
        upload_ttl: Duration::from_secs(900),
        admission_policy: AdmissionPolicy {
            rate_capacity: 5,
            rate_refill_seconds: 60,
            max_pending_per_device: 8,
            byte_quota_24h: 1024,
            pending_max_age_seconds: 30 * 24 * 60 * 60,
            pending_cleanup_limit: 100,
            authorization_cleanup_limit: 500,
            daily_min_interval_seconds: 20 * 60 * 60,
            monthly_min_interval_seconds: 25 * 24 * 60 * 60,
        },
        max_request_concurrency: 16,
        max_db_concurrency: 4,
        global_request_capacity: 1000,
        global_request_refill: Duration::from_secs(1),
    }
}

async fn gateway_with_limits(put_status: StatusCode, limits: GatewayLimits) -> TestGateway {
    gateway_with_put_behavior(put_status, false, limits).await
}

async fn gateway_with_put_behavior(
    put_status: StatusCode,
    store_on_error: bool,
    limits: GatewayLimits,
) -> TestGateway {
    let storage_puts = Arc::new(AtomicUsize::new(0));
    let storage_gets = Arc::new(AtomicUsize::new(0));
    let fail_get_number = Arc::new(AtomicUsize::new(0));
    let storage_bodies = Arc::new(Mutex::new(Vec::new()));
    let mock_storage = MockStorage {
        objects: Arc::new(Mutex::new(StoredObjects::default())),
        put_status,
        store_on_error,
        puts: storage_puts.clone(),
        gets: storage_gets.clone(),
        fail_get_number: fail_get_number.clone(),
        bodies: storage_bodies.clone(),
    };
    let storage = Router::new()
        .route("/{*object}", put(mock_put).get(mock_get))
        .with_state(mock_storage);
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
    let router =
        router(GatewayState::new(database.clone(), b2.clone(), "backups/", limits).unwrap());
    TestGateway {
        directory,
        database,
        router,
        mock_server,
        storage_puts,
        storage_gets,
        fail_get_number,
        storage_bodies,
        b2,
    }
}

async fn mock_put(
    State(storage): State<MockStorage>,
    RoutePath(object): RoutePath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    storage.puts.fetch_add(1, Ordering::SeqCst);
    let bytes = to_bytes(body, 2048).await.unwrap();
    storage.bodies.lock().unwrap().push(bytes.to_vec());
    let valid = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        == Some(bytes.len())
        && headers
            .get("x-amz-meta-sha256")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.len() == 64)
        && headers
            .get("x-amz-server-side-encryption")
            .and_then(|value| value.to_str().ok())
            == Some("AES256");
    if !valid {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::BAD_REQUEST;
        return response;
    }

    let version_id = if storage.put_status.is_success() || storage.store_on_error {
        let mut objects = storage.objects.lock().unwrap();
        objects.next_version += 1;
        let version_id = format!("version-{}", objects.next_version);
        let etag = format!("etag-{}", objects.next_version);
        objects
            .versions
            .entry(object)
            .or_default()
            .push(StoredVersion {
                body: bytes.to_vec(),
                version_id: version_id.clone(),
                etag,
            });
        Some(version_id)
    } else {
        None
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = storage.put_status;
    if storage.put_status.is_success() {
        response
            .headers_mut()
            .insert("x-amz-version-id", version_id.unwrap().parse().unwrap());
    }
    response
}

async fn mock_get(
    State(storage): State<MockStorage>,
    RoutePath(object): RoutePath<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let get_number = storage.gets.fetch_add(1, Ordering::SeqCst) + 1;
    if storage.fail_get_number.load(Ordering::SeqCst) == get_number {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        return response;
    }
    if !headers.contains_key(header::AUTHORIZATION)
        || !headers.contains_key("x-amz-date")
        || headers
            .get("x-amz-content-sha256")
            .and_then(|value| value.to_str().ok())
            != Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        return response;
    }
    let requested_version = query.as_deref().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(name, _)| name == "versionId")
            .map(|(_, value)| value.into_owned())
    });
    let stored = {
        let objects = storage.objects.lock().unwrap();
        objects.versions.get(&object).and_then(|versions| {
            requested_version
                .as_deref()
                .map(|requested| {
                    versions
                        .iter()
                        .find(|version| version.version_id == requested)
                })
                .unwrap_or_else(|| versions.last())
                .cloned()
        })
    };
    let Some(stored) = stored else {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_FOUND;
        return response;
    };
    let mut response = Response::new(Body::from(stored.body.clone()));
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        stored.body.len().to_string().parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("x-amz-server-side-encryption", "AES256".parse().unwrap());
    response
        .headers_mut()
        .insert("x-amz-version-id", stored.version_id.parse().unwrap());
    response
        .headers_mut()
        .insert(header::ETAG, stored.etag.parse().unwrap());
    response
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

async fn direct_upload(upload: &Value, body: &[u8]) -> StatusCode {
    let mut request = reqwest::Client::new().put(upload["upload_url"].as_str().unwrap());
    for (name, value) in upload["required_headers"].as_object().unwrap() {
        request = request.header(name, value.as_str().unwrap());
    }
    request.body(body.to_vec()).send().await.unwrap().status()
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
    assert_eq!(checksum(DIRECT_BODY), GOOD_CHECKSUM);
    let gateway = gateway().await;
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
        Some(json!({ "content_length": DIRECT_BODY.len(), "sha256": GOOD_CHECKSUM })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{upload}");
    assert_eq!(upload["method"], "PUT");
    assert_eq!(upload["retention_class"], "rolling");
    assert_eq!(upload["required_headers"]["content-length"], "4");
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
    assert_eq!(direct_upload(&upload, DIRECT_BODY).await, StatusCode::OK);

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
    assert_eq!(completed["etag"], "etag-1");
    assert_eq!(completed["version_id"], "version-1");
    assert_eq!(gateway.storage_gets.load(Ordering::SeqCst), 1);

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
async fn completion_rejects_same_size_wrong_body_despite_matching_metadata() {
    let gateway = gateway().await;
    let device = gateway.database.issue_device("wrong body", 5).unwrap();
    let (status, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({
            "content_length": DIRECT_BODY.len(),
            "sha256": GOOD_CHECKSUM
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{upload}");
    assert_eq!(direct_upload(&upload, b"evil").await, StatusCode::OK);

    let backup_id = upload["backup_id"].as_str().unwrap();
    let (status, body) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/complete",
        Some(&device.device_token),
        Some(json!({ "backup_id": backup_id })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(gateway.storage_gets.load(Ordering::SeqCst), 1);
    assert_eq!(
        gateway
            .database
            .backup_for_device(&device.device_id, backup_id)
            .unwrap()
            .status,
        "pending"
    );
}

#[tokio::test]
async fn exact_version_download_ignores_later_overwrite_and_removes_bad_partial() {
    let gateway = gateway().await;
    let device = gateway.database.issue_device("download", 5).unwrap();
    let (_, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({
            "content_length": DIRECT_BODY.len(),
            "sha256": GOOD_CHECKSUM
        })),
    )
    .await;
    assert_eq!(direct_upload(&upload, DIRECT_BODY).await, StatusCode::OK);
    let backup_id = upload["backup_id"].as_str().unwrap();
    let (status, completed) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/complete",
        Some(&device.device_token),
        Some(json!({ "backup_id": backup_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["version_id"], "version-1");

    assert_eq!(direct_upload(&upload, b"evil").await, StatusCode::OK);
    let backup = gateway
        .database
        .backup_for_device(&device.device_id, backup_id)
        .unwrap();
    let output = gateway.directory.path().join("recovered.sqlite3.age");
    let verified = gateway
        .b2
        .download_verified(
            &backup.object_key,
            backup.version_id.as_deref().unwrap(),
            backup.size_bytes,
            &backup.sha256,
            &output,
        )
        .await
        .unwrap();
    assert_eq!(verified.version_id, "version-1");
    assert_eq!(std::fs::read(&output).unwrap(), DIRECT_BODY);

    let existing_error = gateway
        .b2
        .download_verified(
            &backup.object_key,
            "version-1",
            backup.size_bytes,
            &backup.sha256,
            &output,
        )
        .await;
    assert!(existing_error.is_err());
    assert_eq!(std::fs::read(&output).unwrap(), DIRECT_BODY);

    let bad_output = gateway.directory.path().join("bad.sqlite3.age");
    let checksum_error = gateway
        .b2
        .download_verified(
            &backup.object_key,
            "version-2",
            backup.size_bytes,
            &backup.sha256,
            &bad_output,
        )
        .await;
    assert!(checksum_error.is_err());
    assert!(!bad_output.exists());
}

#[tokio::test]
async fn identical_pending_long_retention_request_is_resigned_without_new_reservation() {
    let gateway = gateway().await;
    let device = gateway.database.issue_device("daily retry", 5).unwrap();
    let request = json!({
        "content_length": DIRECT_BODY.len(),
        "sha256": GOOD_CHECKSUM,
        "retention_class": "daily"
    });
    let (first_status, first) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(request.clone()),
    )
    .await;
    let (retry_status, retry) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(request),
    )
    .await;
    assert_eq!(first_status, StatusCode::OK, "{first}");
    assert_eq!(retry_status, StatusCode::OK, "{retry}");
    assert_eq!(retry["backup_id"], first["backup_id"]);

    let (different_status, _) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({
            "content_length": DIRECT_BODY.len(),
            "sha256": "aa".repeat(32),
            "retention_class": "daily"
        })),
    )
    .await;
    assert_eq!(different_status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn direct_and_relay_each_consume_quota_while_normal_fallback_fits() {
    let gateway = gateway_with_limits(
        StatusCode::OK,
        GatewayLimits {
            admission_policy: AdmissionPolicy {
                byte_quota_24h: (DIRECT_BODY.len() * 2) as u64,
                ..test_limits().admission_policy
            },
            ..test_limits()
        },
    )
    .await;
    let device = gateway.database.issue_device("quota fallback", 5).unwrap();
    let (_, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({
            "content_length": DIRECT_BODY.len(),
            "sha256": GOOD_CHECKSUM
        })),
    )
    .await;
    let (status, completed) = relay_request(
        &gateway.router,
        upload["backup_id"].as_str().unwrap(),
        &device.device_token,
        DIRECT_BODY.len(),
        DIRECT_BODY,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    let authorized_bytes: u64 = rusqlite::Connection::open(gateway.database.path())
        .unwrap()
        .query_row(
            "SELECT SUM(size_bytes) FROM upload_authorizations WHERE device_id = ?1",
            rusqlite::params![device.device_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(authorized_bytes, (DIRECT_BODY.len() * 2) as u64);
}

#[tokio::test]
async fn relay_quota_rejection_does_not_read_body_or_create_spool() {
    let gateway = gateway_with_limits(
        StatusCode::OK,
        GatewayLimits {
            admission_policy: AdmissionPolicy {
                byte_quota_24h: DIRECT_BODY.len() as u64,
                ..test_limits().admission_policy
            },
            ..test_limits()
        },
    )
    .await;
    let device = gateway.database.issue_device("quota ingress", 5).unwrap();
    let (_, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({
            "content_length": DIRECT_BODY.len(),
            "sha256": GOOD_CHECKSUM
        })),
    )
    .await;
    let backup_id = upload["backup_id"].as_str().unwrap();
    let body_polled = Arc::new(AtomicBool::new(false));
    let observed = body_polled.clone();
    let stream = futures_util::stream::once(async move {
        observed.store(true, Ordering::SeqCst);
        Ok::<Bytes, Infallible>(Bytes::from_static(DIRECT_BODY))
    });
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/v1/backups/relay/{backup_id}"))
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", device.device_token),
        )
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, DIRECT_BODY.len().to_string())
        .body(Body::from_stream(stream))
        .unwrap();
    let response = gateway.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(!body_polled.load(Ordering::SeqCst));
    assert!(relay_spool_is_empty(&gateway));
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn relay_upload_is_authenticated_verified_cleaned_and_idempotent() {
    assert_eq!(checksum(RELAY_BODY), RELAY_CHECKSUM);
    let gateway = gateway().await;
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
    assert_eq!(gateway.storage_gets.load(Ordering::SeqCst), 3);
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
    let (confirmation_status, confirmation) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/complete",
        Some(&device.device_token),
        Some(json!({ "backup_id": backup_id })),
    )
    .await;
    assert_eq!(confirmation_status, StatusCode::OK, "{confirmation}");
    assert_eq!(confirmation["version_id"], completed["version_id"]);
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.storage_gets.load(Ordering::SeqCst), 3);
    assert!(relay_spool_is_empty(&gateway));
}

#[tokio::test]
async fn relay_retry_confirms_ambiguous_put_before_creating_another_version() {
    let gateway = gateway().await;
    gateway.fail_get_number.store(3, Ordering::SeqCst);
    let device = gateway.database.issue_device("ambiguous relay", 5).unwrap();
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

    let (first_status, _) = relay_request(
        &gateway.router,
        backup_id,
        &device.device_token,
        RELAY_BODY.len(),
        RELAY_BODY,
    )
    .await;
    assert_eq!(first_status, StatusCode::BAD_GATEWAY);
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 1);
    assert_eq!(
        gateway
            .database
            .backup_for_device(&device.device_id, backup_id)
            .unwrap()
            .status,
        "pending"
    );

    let (retry_status, completed) = relay_request(
        &gateway.router,
        backup_id,
        &device.device_token,
        RELAY_BODY.len(),
        RELAY_BODY,
    )
    .await;
    assert_eq!(retry_status, StatusCode::OK, "{completed}");
    assert_eq!(completed["version_id"], "version-1");
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 1);
    assert!(relay_spool_is_empty(&gateway));
}

#[tokio::test]
async fn relay_confirms_ambiguous_http_put_response_before_returning() {
    let gateway =
        gateway_with_put_behavior(StatusCode::SERVICE_UNAVAILABLE, true, test_limits()).await;
    let device = gateway
        .database
        .issue_device("ambiguous HTTP relay", 5)
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
    assert_eq!(completed["version_id"], "version-1");
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.storage_gets.load(Ordering::SeqCst), 3);

    let (repeat_status, repeated) = relay_request(
        &gateway.router,
        backup_id,
        &device.device_token,
        RELAY_BODY.len(),
        RELAY_BODY,
    )
    .await;
    assert_eq!(repeat_status, StatusCode::OK, "{repeated}");
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.storage_gets.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn relay_allows_only_one_in_flight_ciphertext_body() {
    let gateway = gateway().await;
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
    let gateway = gateway().await;
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
    let gateway = gateway_with_put_status(StatusCode::SERVICE_UNAVAILABLE).await;
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
    for _ in 0..3 {
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
    assert_eq!(gateway.storage_puts.load(Ordering::SeqCst), 4);
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
    let gateway = gateway().await;
    let device = gateway.database.issue_device("expired relay", 5).unwrap();
    let backup_id = Uuid::new_v4().to_string();
    gateway
        .database
        .reserve_or_reuse_backup(
            &device.device_id,
            &backup_id,
            &format!(
                "backups/rolling/{}/2026/07/15/{backup_id}.sqlite3.age",
                device.device_id
            ),
            RELAY_BODY.len() as u64,
            RELAY_CHECKSUM,
            RetentionClass::Rolling,
            now_epoch() + 900,
            test_limits().admission_policy,
        )
        .unwrap();
    let now = now_epoch();
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
    let gateway = gateway().await;
    let device = gateway.database.issue_device("test", 5).unwrap();

    let (status, _) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        None,
        Some(json!({ "content_length": 4, "sha256": GOOD_CHECKSUM })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({ "content_length": 1025, "sha256": GOOD_CHECKSUM })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, upload) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/upload-url",
        Some(&device.device_token),
        Some(json!({ "content_length": 4, "sha256": GOOD_CHECKSUM })),
    )
    .await;
    let backup_id = upload["backup_id"].as_str().unwrap();
    let (status, missing) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/complete",
        Some(&device.device_token),
        Some(json!({ "backup_id": backup_id })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(missing["error"]["code"], "object_not_present");

    let (stale_status, stale) = json_request(
        &gateway.router,
        "POST",
        "/v1/backups/complete",
        Some(&device.device_token),
        Some(json!({ "backup_id": Uuid::new_v4().to_string() })),
    )
    .await;
    assert_eq!(stale_status, StatusCode::NOT_FOUND);
    assert_eq!(stale["error"]["code"], "not_found");

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
async fn aggregate_rate_limit_runs_before_handlers_but_health_is_bypassed() {
    let gateway = gateway_with_limits(
        StatusCode::OK,
        GatewayLimits {
            global_request_capacity: 1,
            global_request_refill: Duration::from_secs(3600),
            ..test_limits()
        },
    )
    .await;
    let enrollment = json!({
        "enrollment_code": "pen_invalid-but-long-enough-for-db-lookup",
        "device_name": "attacker"
    });
    let (first_status, _) = json_request(
        &gateway.router,
        "POST",
        "/v1/enroll",
        None,
        Some(enrollment.clone()),
    )
    .await;
    let (second_status, second_body) = json_request(
        &gateway.router,
        "POST",
        "/v1/enroll",
        None,
        Some(json!({ "oversized": "x".repeat(20_000) })),
    )
    .await;
    assert_eq!(first_status, StatusCode::UNAUTHORIZED);
    assert_eq!(second_status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second_body["error"]["code"], "rate_limited");

    let health = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let response = gateway.router.clone().oneshot(health).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn request_concurrency_fails_fast_while_stream_permit_is_held() {
    let gateway = gateway_with_limits(
        StatusCode::OK,
        GatewayLimits {
            max_request_concurrency: 1,
            ..test_limits()
        },
    )
    .await;
    let device = gateway.database.issue_device("request limit", 5).unwrap();
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
    let request = Request::builder()
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
    let first = tokio::spawn(async move { first_router.oneshot(request).await.unwrap() });
    started_rx.await.unwrap();

    let (status, body) = json_request(
        &gateway.router,
        "GET",
        "/v1/backups/status",
        Some(&device.device_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "service_unavailable");

    let health = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        gateway
            .router
            .clone()
            .oneshot(health)
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    release_tx.send(()).unwrap();
    assert_eq!(first.await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn health_is_minimal_process_liveness() {
    let gateway = gateway().await;
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
