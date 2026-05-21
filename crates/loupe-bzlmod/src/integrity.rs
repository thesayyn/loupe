//! Subresource Integrity (SRI) hash verification for bzlmod-fetched archives.
//!
//! Format: `algorithm-base64hash` (e.g. `sha256-wLoLQVeHb/8a/so988MhVoaxM6HOYQ3MDYE7Z9pd1TI=`).
//!
//! Adapted from `app/slug_bzlmod/src/integrity.rs` in
//! https://github.com/ZeromatterOSS/slug (Apache-2.0 / MIT,
//! © Meta Platforms, Inc. and affiliates). Slug's `slug_error`-based
//! error type is replaced with `thiserror`; behaviour is otherwise
//! identical.

use base64::Engine;
use sha2::Digest;
use sha2::Sha256;
use sha2::Sha384;
use sha2::Sha512;

#[derive(Debug, thiserror::Error)]
pub enum IntegrityError {
    #[error("invalid integrity format: {0}")]
    InvalidFormat(String),

    #[error("unsupported hash algorithm: {0}")]
    UnsupportedAlgorithm(String),

    #[error("invalid base64 encoding in integrity hash")]
    InvalidBase64,

    #[error(
        "integrity mismatch: expected {expected}, computed {computed} (algorithm: {algorithm})"
    )]
    Mismatch {
        algorithm: String,
        expected: String,
        computed: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlgorithm {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, IntegrityError> {
        match s.to_lowercase().as_str() {
            "sha256" => Ok(HashAlgorithm::Sha256),
            "sha384" => Ok(HashAlgorithm::Sha384),
            "sha512" => Ok(HashAlgorithm::Sha512),
            _ => Err(IntegrityError::UnsupportedAlgorithm(s.to_string())),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HashAlgorithm::Sha256 => "sha256",
            HashAlgorithm::Sha384 => "sha384",
            HashAlgorithm::Sha512 => "sha512",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    pub algorithm: HashAlgorithm,
    pub hash: String,
}

impl Integrity {
    pub fn parse(integrity: &str) -> Result<Self, IntegrityError> {
        let (algo, hash) = integrity
            .split_once('-')
            .ok_or_else(|| IntegrityError::InvalidFormat(integrity.to_string()))?;

        let algorithm = HashAlgorithm::from_str(algo)?;

        base64::engine::general_purpose::STANDARD
            .decode(hash)
            .map_err(|_| IntegrityError::InvalidBase64)?;

        Ok(Self {
            algorithm,
            hash: hash.to_string(),
        })
    }

    pub fn compute(algorithm: HashAlgorithm, data: &[u8]) -> Self {
        let hash = match algorithm {
            HashAlgorithm::Sha256 => {
                let mut hasher = Sha256::new();
                hasher.update(data);
                base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
            }
            HashAlgorithm::Sha384 => {
                let mut hasher = Sha384::new();
                hasher.update(data);
                base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
            }
            HashAlgorithm::Sha512 => {
                let mut hasher = Sha512::new();
                hasher.update(data);
                base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
            }
        };

        Self { algorithm, hash }
    }

    pub fn to_sri_string(&self) -> String {
        format!("{}-{}", self.algorithm.as_str(), self.hash)
    }

    pub fn verify(&self, data: &[u8]) -> Result<(), IntegrityError> {
        let computed = Self::compute(self.algorithm, data);

        if computed.hash == self.hash {
            Ok(())
        } else {
            Err(IntegrityError::Mismatch {
                algorithm: self.algorithm.as_str().to_string(),
                expected: self.hash.clone(),
                computed: computed.hash,
            })
        }
    }
}

pub fn verify_integrity(data: &[u8], expected: &str) -> Result<(), IntegrityError> {
    let integrity = Integrity::parse(expected)?;
    integrity.verify(data)
}

pub fn compute_integrity(data: &[u8], algorithm: HashAlgorithm) -> String {
    Integrity::compute(algorithm, data).to_sri_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DATA: &[u8] = b"Hello, World!";
    const TEST_SHA256: &str = "sha256-3/1gIbsr1bCvZ2KQgJ7DpTGR3YHH9wpLKGiKNiGCmG8=";

    #[test]
    fn parse_sha256() {
        let integrity = Integrity::parse(TEST_SHA256).unwrap();
        assert_eq!(integrity.algorithm, HashAlgorithm::Sha256);
        assert_eq!(
            integrity.hash,
            "3/1gIbsr1bCvZ2KQgJ7DpTGR3YHH9wpLKGiKNiGCmG8="
        );
    }

    #[test]
    fn parse_invalid_format() {
        assert!(Integrity::parse("not-valid-format").is_err());
        assert!(Integrity::parse("sha256").is_err());
        assert!(Integrity::parse("").is_err());
    }

    #[test]
    fn parse_unsupported_algorithm() {
        let result = Integrity::parse("md5-abc123");
        assert!(matches!(
            result,
            Err(IntegrityError::UnsupportedAlgorithm(_))
        ));
    }

    #[test]
    fn parse_invalid_base64() {
        let result = Integrity::parse("sha256-not!valid!base64!!");
        assert!(matches!(result, Err(IntegrityError::InvalidBase64)));
    }

    #[test]
    fn compute_sha256() {
        let integrity = Integrity::compute(HashAlgorithm::Sha256, TEST_DATA);
        assert_eq!(integrity.algorithm, HashAlgorithm::Sha256);
        assert_eq!(
            integrity.hash,
            "3/1gIbsr1bCvZ2KQgJ7DpTGR3YHH9wpLKGiKNiGCmG8="
        );
    }

    #[test]
    fn verify_success() {
        verify_integrity(TEST_DATA, TEST_SHA256).unwrap();
    }

    #[test]
    fn verify_failure() {
        let wrong_data = b"Wrong data";
        let result = verify_integrity(wrong_data, TEST_SHA256);
        assert!(result.is_err());
    }

    #[test]
    fn to_sri_string_roundtrip() {
        let integrity = Integrity::compute(HashAlgorithm::Sha256, TEST_DATA);
        assert_eq!(integrity.to_sri_string(), TEST_SHA256);
    }

    #[test]
    fn sha384_roundtrip() {
        let data = b"test";
        let integrity = Integrity::compute(HashAlgorithm::Sha384, data);
        assert_eq!(integrity.algorithm, HashAlgorithm::Sha384);

        let parsed = Integrity::parse(&integrity.to_sri_string()).unwrap();
        assert_eq!(parsed.algorithm, HashAlgorithm::Sha384);
        parsed.verify(data).unwrap();
    }

    #[test]
    fn sha512_roundtrip() {
        let data = b"test";
        let integrity = Integrity::compute(HashAlgorithm::Sha512, data);
        assert_eq!(integrity.algorithm, HashAlgorithm::Sha512);

        let parsed = Integrity::parse(&integrity.to_sri_string()).unwrap();
        assert_eq!(parsed.algorithm, HashAlgorithm::Sha512);
        parsed.verify(data).unwrap();
    }
}
