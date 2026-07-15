#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{collections::BTreeMap, path::Path};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, CONTENT_LENGTH, ETAG};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
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
    pub version_id: String,
    pub size_bytes: u64,
    pub sha256: String,
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

    /// Upload an already encrypted, locally verified spool file and then prove
    /// the stored object body through the same signed streaming GET used by
    /// direct uploads. Storage credentials and signed URLs never enter errors.
    pub async fn upload_ciphertext(
        &self,
        object_key: &str,
        expected_size: u64,
        expected_sha256: &str,
        path: &Path,
    ) -> Result<VerifiedObject> {
        // An earlier direct or relay PUT may have reached B2 even if its
        // response was lost. Only an authenticated 404 permits another PUT.
        if let Some(verified) = self
            .verify_object_if_present(object_key, expected_size, expected_sha256)
            .await?
        {
            return Ok(verified);
        }
        let upload = self.presign_put(object_key, expected_size, expected_sha256, Utc::now())?;
        let file = tokio::fs::File::open(path)
            .await
            .map_err(AppError::internal)?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
        // The shared client keeps ordinary control-plane requests short. A bounded
        // ciphertext relay can legitimately need much longer for the maximum
        // 256 MiB object, matching the desktop and reverse-proxy upload bound.
        let mut request = self
            .client
            .put(upload.url)
            .timeout(std::time::Duration::from_secs(15 * 60));
        for (name, value) in upload.headers {
            request = request.header(name, value);
        }

        let response = match request.body(body).send().await {
            Ok(response) => response,
            Err(error) => {
                let kind = if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connection failure"
                } else {
                    "transport failure"
                };
                return self
                    .confirm_ambiguous_put(
                        object_key,
                        expected_size,
                        expected_sha256,
                        &format!("B2 PUT {kind}"),
                    )
                    .await;
            }
        };
        let status = response.status();
        if !status.is_success() {
            drop(response);
            if status == reqwest::StatusCode::REQUEST_TIMEOUT
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error()
            {
                return self
                    .confirm_ambiguous_put(
                        object_key,
                        expected_size,
                        expected_sha256,
                        &format!("B2 PUT returned HTTP {}", status.as_u16()),
                    )
                    .await;
            }
            return Err(AppError::Upstream(format!(
                "B2 PUT returned HTTP {}",
                status.as_u16()
            )));
        }
        drop(response);

        self.verify_object(object_key, expected_size, expected_sha256)
            .await
    }

    async fn confirm_ambiguous_put(
        &self,
        object_key: &str,
        expected_size: u64,
        expected_sha256: &str,
        failure: &str,
    ) -> Result<VerifiedObject> {
        match self
            .verify_object_if_present(object_key, expected_size, expected_sha256)
            .await
        {
            Ok(Some(verified)) => Ok(verified),
            Ok(None) => Err(AppError::Upstream(format!(
                "{failure}; confirmation GET found no stored object"
            ))),
            Err(_) => Err(AppError::Upstream(format!(
                "{failure}; confirmation GET could not establish object state"
            ))),
        }
    }

    /// Stream the current immutable reservation key from storage, bound the
    /// response to the reserved size, hash the actual bytes, and capture the
    /// exact storage version that supplied those bytes.
    pub async fn verify_object(
        &self,
        object_key: &str,
        expected_size: u64,
        expected_sha256: &str,
    ) -> Result<VerifiedObject> {
        self.verify_object_if_present(object_key, expected_size, expected_sha256)
            .await?
            .ok_or_else(|| AppError::Upstream("B2 GET found no stored object".to_owned()))
    }

    pub async fn verify_object_if_present(
        &self,
        object_key: &str,
        expected_size: u64,
        expected_sha256: &str,
    ) -> Result<Option<VerifiedObject>> {
        self.get_verified_object(object_key, None, expected_size, expected_sha256, None)
            .await
    }

    /// Download one exact completed object version into a newly created output
    /// file. A short/mutated response or any write failure removes the partial
    /// file; an existing path is never replaced.
    pub async fn download_verified(
        &self,
        object_key: &str,
        version_id: &str,
        expected_size: u64,
        expected_sha256: &str,
        output_path: &Path,
    ) -> Result<VerifiedObject> {
        validate_version_id(version_id)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let standard_file = options.open(output_path).map_err(AppError::internal)?;
        let mut output = tokio::fs::File::from_std(standard_file);
        let result = self
            .get_verified_object(
                object_key,
                Some(version_id),
                expected_size,
                expected_sha256,
                Some(&mut output),
            )
            .await
            .and_then(|verified| {
                verified
                    .ok_or_else(|| AppError::Upstream("B2 GET found no stored object".to_owned()))
            });
        match result {
            Ok(verified) => {
                let durable = async {
                    output.flush().await.map_err(AppError::internal)?;
                    output.sync_all().await.map_err(AppError::internal)
                }
                .await;
                drop(output);
                if let Err(error) = durable {
                    remove_partial_output(output_path).await?;
                    return Err(error);
                }
                Ok(verified)
            }
            Err(error) => {
                drop(output);
                remove_partial_output(output_path).await?;
                Err(error)
            }
        }
    }

    async fn get_verified_object(
        &self,
        object_key: &str,
        requested_version_id: Option<&str>,
        expected_size: u64,
        expected_sha256: &str,
        mut output: Option<&mut tokio::fs::File>,
    ) -> Result<Option<VerifiedObject>> {
        validate_object_key(object_key)?;
        if let Some(version_id) = requested_version_id {
            validate_version_id(version_id)?;
        }
        let mut url = self.object_url(object_key)?;
        let now = Utc::now();
        let host = canonical_host(&url)?;
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let short_date = now.format("%Y%m%d").to_string();
        let scope = format!("{short_date}/{}/s3/aws4_request", self.config.region);
        let mut query = requested_version_id
            .map(|version_id| vec![("versionId", version_id.to_owned())])
            .unwrap_or_default();
        let canonical_query = canonical_query(&mut query);
        if canonical_query.is_empty() {
            url.set_query(None);
        } else {
            url.set_query(Some(&canonical_query));
        }
        let mut signed = BTreeMap::new();
        signed.insert("host".to_owned(), host);
        signed.insert("x-amz-content-sha256".to_owned(), EMPTY_SHA256.to_owned());
        signed.insert("x-amz-date".to_owned(), amz_date.clone());
        let signed_headers = signed.keys().cloned().collect::<Vec<_>>().join(";");
        let canonical_request = format!(
            "GET\n{}\n{canonical_query}\n{}\n{signed_headers}\n{EMPTY_SHA256}",
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
            .get(url)
            .timeout(std::time::Duration::from_secs(15 * 60))
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
                AppError::Upstream(format!("B2 GET {kind}"))
            })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "B2 GET returned HTTP {}",
                response.status().as_u16()
            )));
        }

        verify_response_headers(response.headers(), expected_size)?;
        let etag = bounded_header(response.headers(), ETAG.as_str(), 256)?;
        let version_id = required_bounded_header(response.headers(), "x-amz-version-id", 256)?;
        if let Some(requested) = requested_version_id {
            if version_id != requested {
                return Err(AppError::Upstream(
                    "B2 returned a different object version".to_owned(),
                ));
            }
        }

        let mut stream = response.bytes_stream();
        let mut received = 0_u64;
        let mut hasher = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                let kind = if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connection failure"
                } else {
                    "body stream failure"
                };
                AppError::Upstream(format!("B2 GET {kind}"))
            })?;
            received = received
                .checked_add(u64::try_from(chunk.len()).map_err(AppError::internal)?)
                .ok_or_else(|| AppError::Upstream("B2 object size overflow".to_owned()))?;
            if received > expected_size {
                return Err(AppError::Upstream(
                    "B2 object exceeded the reserved size".to_owned(),
                ));
            }
            hasher.update(&chunk);
            if let Some(file) = output.as_deref_mut() {
                file.write_all(&chunk).await.map_err(AppError::internal)?;
            }
        }
        if received != expected_size {
            return Err(AppError::Upstream(format!(
                "B2 object size mismatch (expected {expected_size}, got {received})"
            )));
        }
        let actual_sha256 = hex::encode(hasher.finalize());
        if actual_sha256 != expected_sha256 {
            return Err(AppError::Upstream(
                "B2 object body checksum did not match the reservation".to_owned(),
            ));
        }
        Ok(Some(VerifiedObject {
            etag,
            version_id,
            size_bytes: received,
            sha256: actual_sha256,
        }))
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

fn verify_response_headers(headers: &HeaderMap, expected_size: u64) -> Result<()> {
    let size = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| AppError::Upstream("B2 GET omitted a valid Content-Length".to_owned()))?;
    if size != expected_size {
        return Err(AppError::Upstream(format!(
            "B2 object size mismatch (expected {expected_size}, got {size})"
        )));
    }
    let encryption = headers
        .get("x-amz-server-side-encryption")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::Upstream("B2 GET omitted SSE status".to_owned()))?;
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

fn required_bounded_header(headers: &HeaderMap, name: &str, max_len: usize) -> Result<String> {
    let value = bounded_header(headers, name, max_len)?.ok_or_else(|| {
        AppError::Upstream(format!("B2 GET omitted required response header {name}"))
    })?;
    if value.is_empty() {
        return Err(AppError::Upstream(format!(
            "B2 GET returned an empty required response header {name}"
        )));
    }
    Ok(value)
}

fn validate_version_id(version_id: &str) -> Result<()> {
    if version_id.is_empty() || version_id.len() > 256 || version_id.chars().any(char::is_control) {
        return Err(AppError::BadRequest("B2 version ID is invalid"));
    }
    Ok(())
}

async fn remove_partial_output(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::internal(error)),
    }
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

    use super::*;
    use chrono::TimeZone;
    use reqwest::header::HeaderValue;

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
    fn required_storage_version_header_rejects_missing_and_empty_values() {
        let missing = HeaderMap::new();
        assert!(matches!(
            required_bounded_header(&missing, "x-amz-version-id", 256),
            Err(AppError::Upstream(_))
        ));

        let mut empty = HeaderMap::new();
        empty.insert("x-amz-version-id", HeaderValue::from_static(""));
        assert!(matches!(
            required_bounded_header(&empty, "x-amz-version-id", 256),
            Err(AppError::Upstream(_))
        ));
    }
}
