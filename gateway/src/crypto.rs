use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{AppError, Result};

type HmacSha256 = Hmac<Sha256>;

pub fn generate_secret(prefix: &str, random_bytes: usize) -> Result<String> {
    let mut bytes = vec![0_u8; random_bytes];
    getrandom::fill(&mut bytes).map_err(AppError::internal)?;
    Ok(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

pub fn hash_secret(pepper: &[u8], purpose: &[u8], secret: &str) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(pepper)
        .expect("HMAC accepts keys of any non-zero length; configuration enforces a pepper");
    mac.update(purpose);
    mac.update(&[0]);
    mac.update(secret.as_bytes());
    mac.finalize().into_bytes().into()
}

pub fn normalize_sha256(value: &str) -> Result<String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest(
            "sha256 must be exactly 64 hexadecimal characters",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

pub fn validate_name(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > 120
        || trimmed.chars().any(|character| character.is_control())
    {
        return Err(AppError::BadRequest(
            "name must contain 1-120 non-control UTF-8 bytes",
        ));
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_secrets_are_prefixed_and_unique() {
        let first = generate_secret("pdt_", 32).unwrap();
        let second = generate_secret("pdt_", 32).unwrap();
        assert!(first.starts_with("pdt_"));
        assert_ne!(first, second);
    }

    #[test]
    fn hashes_are_bound_to_their_purpose() {
        let pepper = b"a sufficiently long test-only pepper";
        assert_ne!(
            hash_secret(pepper, b"device", "same"),
            hash_secret(pepper, b"enrollment", "same")
        );
    }

    #[test]
    fn sha256_is_validated_and_normalized() {
        assert_eq!(normalize_sha256(&"AB".repeat(32)).unwrap(), "ab".repeat(32));
        assert!(normalize_sha256("xyz").is_err());
    }
}
