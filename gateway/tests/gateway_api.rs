use std::{sync::Arc, time::Duration};

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, Response, StatusCode},
    routing::head,
    Router,
};
use pusula_backup_gateway::{
    api::{router, GatewayState},
    b2::B2Client,
    config::B2Config,
    db::Database,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;
use url::Url;

const CHECKSUM: &str = "abababababababababababababababababababababababababababababababab";

struct TestGateway {
    _directory: TempDir,
    database: Database,
    router: Router,
    mock_server: tokio::task::JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.mock_server.abort();
    }
}

async fn gateway(object_size: u64, object_checksum: &'static str) -> TestGateway {
    let storage = Router::new().route(
        "/{*object}",
        head(move || async move {
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
        _directory: directory,
        database,
        router,
        mock_server,
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
