use std::collections::BTreeMap;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, CONTENT_LENGTH, ETAG};
use serde::Serialize;
use sha2::{Digest, Sha256};
use url::{Host, Url};

use crate::{
    config::B2Config,
    error::{AppError, Result},
};

type HmacSha256 = Hmac<Sha256>;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

#[derive(Clone)]
pub struct B2Client {
    config: B2Config,
    client: reqwest::Client,
}

#[derive(Debug, Clone, Serialize)]
pub struct PresignedUpload {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub expires_at: String,
}

#[derive(Debug, Clone)]
pub struct VerifiedObject {
    pub etag: Option<String>,
    pub version_id: Option<String>,
}

impl B2Client {
    pub fn new(config: B2Config, client: reqwest::Client) -> Self {
        Self { config, client }
    }

    pub fn presign_put(
        &self,
        object_key: &str,
        content_length: u64,
        sha256: &str,
        now: DateTime<Utc>,
    ) -> Result<PresignedUpload> {
        validate_object_key(object_key)?;
        let url = self.object_url(object_key)?;
        let host = canonical_host(&url)?;
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let short_date = now.format("%Y%m%d").to_string();
        let scope = format!("{short_date}/{}/s3/aws4_request", self.config.region);

        let mut headers = BTreeMap::new();
        headers.insert("content-length".to_owned(), content_length.to_string());
        headers.insert("host".to_owned(), host);
        headers.insert(
            "x-amz-content-sha256".to_owned(),
            UNSIGNED_PAYLOAD.to_owned(),
        );
        headers.insert("x-amz-meta-sha256".to_owned(), sha256.to_owned());
        headers.insert(
            "x-amz-server-side-encryption".to_owned(),
            "AES256".to_owned(),
        );
        let signed_headers = headers.keys().cloned().collect::<Vec<_>>().join(";");
        let canonical_headers = canonical_headers(&headers);
        let expires = self.config.presign_ttl.as_secs();
        let mut query = vec![
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_owned()),
            (
                "X-Amz-Credential",
                format!("{}/{scope}", self.config.key_id),
            ),
            ("X-Amz-Date", amz_date.clone()),
            ("X-Amz-Expires", expires.to_string()),
            ("X-Amz-SignedHeaders", signed_headers.clone()),
        ];
        let canonical_query = canonical_query(&mut query);
        let canonical_request = format!(
            "PUT\n{}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{UNSIGNED_PAYLOAD}",
            url.path(),
        );
        let signature = self.signature(&short_date, &amz_date, &scope, &canonical_request);
        let upload_url = format!(
            "{}?{canonical_query}&X-Amz-Signature={}",
            url.as_str(),
            hex::encode(signature)
        );
        headers.remove("host");

        let expires_at =
            now + Duration::from_std(self.config.presign_ttl).map_err(AppError::internal)?;
        Ok(PresignedUpload {
            url: upload_url,
            headers,
            expires_at: expires_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        })
    }

    pub async fn verify_object(
        &self,
        object_key: &str,
        expected_size: u64,
        expected_sha256: &str,
    ) -> Result<VerifiedObject> {
        validate_object_key(object_key)?;
        let url = self.object_url(object_key)?;
        let now = Utc::now();
        let host = canonical_host(&url)?;
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let short_date = now.format("%Y%m%d").to_string();
        let scope = format!("{short_date}/{}/s3/aws4_request", self.config.region);
        let mut signed = BTreeMap::new();
        signed.insert("host".to_owned(), host);
        signed.insert("x-amz-content-sha256".to_owned(), EMPTY_SHA256.to_owned());
        signed.insert("x-amz-date".to_owned(), amz_date.clone());
        let signed_headers = signed.keys().cloned().collect::<Vec<_>>().join(";");
        let canonical_request = format!(
            "HEAD\n{}\n\n{}\n{signed_headers}\n{EMPTY_SHA256}",
            url.path(),
            canonical_headers(&signed)
        );
        let signature = self.signature(&short_date, &amz_date, &scope, &canonical_request);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={}",
            self.config.key_id,
            hex::encode(signature)
        );

        let response = self
            .client
            .head(url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", EMPTY_SHA256)
            .header("authorization", authorization)
            .send()
            .await
            .map_err(|error| {
                let kind = if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connection failure"
                } else {
                    "transport failure"
                };
                AppError::Upstream(format!("B2 HEAD {kind}"))
            })?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "B2 HEAD returned HTTP {}",
                response.status().as_u16()
            )));
        }

        verify_headers(response.headers(), expected_size, expected_sha256)?;
        Ok(VerifiedObject {
            etag: bounded_header(response.headers(), ETAG.as_str(), 256)?,
            version_id: bounded_header(response.headers(), "x-amz-version-id", 256)?,
        })
    }

    fn object_url(&self, object_key: &str) -> Result<Url> {
        let mut url = self.config.endpoint.clone();
        let base_path = url.path().trim_end_matches('/');
        let path = format!(
            "{base_path}/{}/{}",
            self.config.bucket,
            object_key.trim_start_matches('/')
        );
        url.set_path(&path);
        Ok(url)
    }

    fn signature(
        &self,
        short_date: &str,
        amz_date: &str,
        scope: &str,
        canonical_request: &str,
    ) -> [u8; 32] {
        let request_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{request_hash}");
        let date_key = hmac(
            format!("AWS4{}", self.config.application_key).as_bytes(),
            short_date.as_bytes(),
        );
        let region_key = hmac(&date_key, self.config.region.as_bytes());
        let service_key = hmac(&region_key, b"s3");
        let signing_key = hmac(&service_key, b"aws4_request");
        hmac(&signing_key, string_to_sign.as_bytes())
    }
}

fn canonical_host(url: &Url) -> Result<String> {
    let host = match url.host() {
        Some(Host::Domain(domain)) => domain.to_owned(),
        Some(Host::Ipv4(address)) => address.to_string(),
        Some(Host::Ipv6(address)) => format!("[{address}]"),
        None => return Err(AppError::BadRequest("B2 endpoint has no host")),
    };
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn canonical_headers(headers: &BTreeMap<String, String>) -> String {
    headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect()
}

fn canonical_query(query: &mut [(&str, String)]) -> String {
    let mut encoded = query
        .iter()
        .map(|(name, value)| (aws_encode(name), aws_encode(value)))
        .collect::<Vec<_>>();
    encoded.sort_unstable();
    encoded
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn aws_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn hmac(key: &[u8], value: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(value);
    mac.finalize().into_bytes().into()
}

fn verify_headers(headers: &HeaderMap, expected_size: u64, expected_sha256: &str) -> Result<()> {
    let size = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| AppError::Upstream("B2 HEAD omitted a valid Content-Length".to_owned()))?;
    if size != expected_size {
        return Err(AppError::Upstream(format!(
            "B2 object size mismatch (expected {expected_size}, got {size})"
        )));
    }
    let object_sha256 = headers
        .get("x-amz-meta-sha256")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::Upstream("B2 HEAD omitted checksum metadata".to_owned()))?;
    if !object_sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err(AppError::Upstream(
            "B2 object checksum metadata did not match".to_owned(),
        ));
    }
    let encryption = headers
        .get("x-amz-server-side-encryption")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::Upstream("B2 HEAD omitted SSE status".to_owned()))?;
    if encryption != "AES256" {
        return Err(AppError::Upstream(
            "B2 object is not protected by SSE-B2".to_owned(),
        ));
    }
    Ok(())
}

fn bounded_header(headers: &HeaderMap, name: &str, max_len: usize) -> Result<Option<String>> {
    headers
        .get(name)
        .map(|value| {
            let value = value
                .to_str()
                .map_err(|_| AppError::Upstream(format!("B2 returned an invalid {name}")))?;
            if value.len() > max_len || value.chars().any(char::is_control) {
                return Err(AppError::Upstream(format!("B2 returned an unsafe {name}")));
            }
            Ok(value.to_owned())
        })
        .transpose()
}

fn validate_object_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > 900
        || key.starts_with('/')
        || key.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(AppError::Internal(
            "generated B2 object key was invalid".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::TimeZone;
    use reqwest::header::HeaderValue;

    use super::*;

    fn client() -> B2Client {
        B2Client::new(
            B2Config {
                endpoint: Url::parse("https://s3.us-west-004.backblazeb2.com").unwrap(),
                region: "us-west-004".to_owned(),
                bucket: "stronganchor-pusula-desktop-backups".to_owned(),
                prefix: "backups/".to_owned(),
                key_id: "test-key-id".to_owned(),
                application_key: "test-application-key".to_owned(),
                presign_ttl: Duration::from_secs(900),
            },
            reqwest::Client::new(),
        )
    }

    #[test]
    fn presigned_put_is_scoped_and_does_not_leak_secret() {
        let now = Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, 0).unwrap();
        let upload = client()
            .presign_put(
                "backups/device/2026/07/14/backup.sqlite3.age",
                1234,
                &"ab".repeat(32),
                now,
            )
            .unwrap();
        assert!(upload.url.contains("X-Amz-Expires=900"));
        assert!(upload.url.contains("X-Amz-Signature="));
        assert!(!upload.url.contains("test-application-key"));
        assert_eq!(upload.headers["content-length"], "1234");
        assert_eq!(upload.headers["x-amz-content-sha256"], UNSIGNED_PAYLOAD);
        assert_eq!(upload.headers["x-amz-server-side-encryption"], "AES256");
        assert_eq!(upload.expires_at, "2026-07-14T12:15:00Z");
    }

    #[test]
    fn object_header_verification_is_strict() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("100"));
        headers.insert("x-amz-meta-sha256", HeaderValue::from_static("abcd"));
        headers.insert(
            "x-amz-server-side-encryption",
            HeaderValue::from_static("AES256"),
        );
        assert!(verify_headers(&headers, 100, "abcd").is_ok());
        assert!(verify_headers(&headers, 101, "abcd").is_err());
    }
}
