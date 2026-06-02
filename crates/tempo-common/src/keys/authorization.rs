//! Key authorization: decode, validate, and sign key authorizations.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::{
    primitives::Address,
    rlp::{Decodable, Encodable},
    signers::{local::PrivateKeySigner, SignerSync},
};
use tempo_primitives::transaction::{
    KeyAuthorization, PrimitiveSignature, SignatureType, SignedKeyAuthorization, TokenLimit,
};

use super::{KeyType, StoredTokenLimit};
use crate::error::{ConfigError, KeyError, TempoError};

/// Default key authorization expiry: 30 days.
const DEFAULT_EXPIRY_SECS: u64 = 30 * 24 * 60 * 60;

/// Default per-token spending limit: $100 (6 decimals).
const DEFAULT_LIMIT: u64 = 100_000_000;

/// Decoded and validated key authorization.
#[derive(Debug, PartialEq, Eq)]
pub struct ValidatedKeyAuth {
    pub hex: String,
    pub expiry: u64,
    pub chain_id: u64,
    pub key_type: KeyType,
    pub limits: Vec<StoredTokenLimit>,
}

/// Decode a hex-encoded `SignedKeyAuthorization`.
///
/// Accepts hex strings with or without a "0x" prefix.
/// Logs a warning if the input is present but fails to decode.
pub fn decode(hex_str: &str) -> Option<SignedKeyAuthorization> {
    let raw = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = match hex::decode(raw) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("Invalid key authorization hex: {e}");
            return None;
        }
    };
    let mut slice = bytes.as_slice();
    match SignedKeyAuthorization::decode(&mut slice) {
        Ok(auth) => Some(auth),
        Err(e) => {
            tracing::warn!("Invalid key authorization RLP: {e}");
            None
        }
    }
}

/// Validate a key authorization hex string against an expected key ID.
///
/// # Errors
///
/// Returns an error when the authorization payload is malformed or targets a
/// different key than `expected_key_id`.
pub fn validate(
    hex_str: Option<&str>,
    expected_key_id: Address,
) -> Result<Option<ValidatedKeyAuth>, TempoError> {
    let Some(hex_str) = hex_str else {
        return Ok(None);
    };

    let signed = decode(hex_str).ok_or(ConfigError::InvalidKeyAuthorization)?;

    if signed.authorization.key_id != expected_key_id {
        return Err(ConfigError::InvalidAddress {
            context: "key authorization target",
            value: format!(
                "{:#x} (expected {:#x})",
                signed.authorization.key_id, expected_key_id
            ),
        }
        .into());
    }

    let expiry = signed.authorization.expiry.map_or(0, |e| e.get());
    let chain_id = signed.authorization.chain_id;

    let key_type = match signed.authorization.key_type {
        SignatureType::Secp256k1 => KeyType::Secp256k1,
        SignatureType::P256 => KeyType::P256,
        SignatureType::WebAuthn => KeyType::WebAuthn,
    };

    let limits = signed
        .authorization
        .limits
        .iter()
        .flatten()
        .map(|tl| StoredTokenLimit {
            currency: tl.token,
            limit: tl.limit.to_string(),
        })
        .collect();

    Ok(Some(ValidatedKeyAuth {
        hex: hex_str.to_string(),
        expiry,
        chain_id,
        key_type,
        limits,
    }))
}

/// Sign a key authorization for a key using the wallet EOA.
///
/// Returns the validated auth containing hex, expiry, and token limits.
/// Uses $100 USDC.e limit and 30-day expiry.
///
/// # Errors
///
/// Returns an error when the chain ID is unsupported or the authorization
/// signature operation fails.
pub fn sign(
    wallet_signer: &PrivateKeySigner,
    access_signer: &PrivateKeySigner,
    chain_id: u64,
) -> Result<ValidatedKeyAuth, TempoError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs();
    let expiry_secs = now + DEFAULT_EXPIRY_SECS;
    let limit = alloy::primitives::U256::from(DEFAULT_LIMIT);
    // Authorize the canonical stablecoin for this network.
    let network = crate::network::NetworkId::from_chain_id(chain_id)
        .ok_or_else(|| TempoError::from(ConfigError::UnsupportedChainId(chain_id)))?;
    let token_addrs = [network.token().address];
    let token_limits: Vec<TokenLimit> = token_addrs
        .iter()
        .map(|&token| TokenLimit {
            token,
            limit,
            period: 0,
        })
        .collect();
    let stored_limits: Vec<StoredTokenLimit> = token_addrs
        .iter()
        .map(|&addr| StoredTokenLimit {
            currency: addr,
            limit: limit.to_string(),
        })
        .collect();
    let auth = KeyAuthorization {
        chain_id,
        key_type: SignatureType::Secp256k1,
        key_id: access_signer.address(),
        expiry: std::num::NonZero::new(expiry_secs),
        limits: Some(token_limits),
        allowed_calls: None,
        witness: None,
        is_admin: false,
        account: None,
    };
    let sig = wallet_signer
        .sign_hash_sync(&auth.signature_hash())
        .map_err(|source| {
            TempoError::from(KeyError::SigningOperationSource {
                operation: "sign key authorization",
                source: Box::new(source),
            })
        })?;
    let signed = auth.into_signed(PrimitiveSignature::Secp256k1(sig));
    let mut buf = Vec::new();
    signed.encode(&mut buf);
    Ok(ValidatedKeyAuth {
        hex: format!("0x{}", hex::encode(&buf)),
        expiry: expiry_secs,
        chain_id,
        key_type: KeyType::Secp256k1,
        limits: stored_limits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== decode tests ====================

    #[test]
    fn test_decode_none_on_invalid_hex() {
        assert!(decode("not-valid-hex").is_none());
    }

    #[test]
    fn test_decode_none_on_invalid_rlp() {
        assert!(decode("deadbeef").is_none());
    }

    #[test]
    fn test_decode_none_on_empty() {
        assert!(decode("").is_none());
    }

    #[test]
    fn test_decode_strips_0x_prefix() {
        assert!(decode("0xdeadbeef").is_none());
    }

    // ==================== validate tests ====================

    fn make_signed_auth_hex(key_id: Address) -> String {
        let signer: PrivateKeySigner =
            "0x1234567890123456789012345678901234567890123456789012345678901234"
                .parse()
                .unwrap();

        let auth = KeyAuthorization {
            chain_id: 42431,
            key_type: SignatureType::Secp256k1,
            key_id,
            expiry: std::num::NonZero::new(9_999_999_999),
            limits: None,
            allowed_calls: None,
            witness: None,
            is_admin: false,
            account: None,
        };

        let sig = signer.sign_hash_sync(&auth.signature_hash()).unwrap();
        let signed_auth = auth.into_signed(PrimitiveSignature::Secp256k1(sig));

        let mut buf = Vec::new();
        signed_auth.encode(&mut buf);
        format!("0x{}", hex::encode(&buf))
    }

    #[test]
    fn test_validate_matching_key_id() {
        let signer = PrivateKeySigner::random();
        let hex = make_signed_auth_hex(signer.address());
        let result = validate(Some(&hex), signer.address());
        assert!(result.is_ok());
        let validated = result.unwrap().unwrap();
        assert_eq!(validated.hex, hex);
        assert_eq!(validated.expiry, 9_999_999_999);
        assert_eq!(validated.chain_id, 42431);
        assert_eq!(validated.key_type, KeyType::Secp256k1);
    }

    #[test]
    fn test_validate_mismatched_key_id() {
        let signer = PrivateKeySigner::random();
        let wrong_address = Address::repeat_byte(0xFF);
        let hex = make_signed_auth_hex(wrong_address);
        let result = validate(Some(&hex), signer.address());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("key authorization target"));
        assert!(err.contains("expected"));
    }

    #[test]
    fn test_validate_none() {
        let result = validate(None, Address::ZERO);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_validate_invalid_hex() {
        let result = validate(Some("not-hex"), Address::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_invalid_rlp() {
        let result = validate(Some("0xdeadbeef"), Address::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn test_sign_uses_per_network_token() {
        let wallet = PrivateKeySigner::random();
        let access = PrivateKeySigner::random();

        let tempo_auth = sign(&wallet, &access, 4217).unwrap();
        let moderato_auth = sign(&wallet, &access, 42431).unwrap();

        // Different networks should authorize different token addresses
        assert_ne!(
            tempo_auth.limits[0].currency,
            moderato_auth.limits[0].currency
        );
        // Verify correct tokens
        assert_eq!(tempo_auth.limits[0].currency, crate::network::USDCE_TOKEN);
        assert_eq!(
            moderato_auth.limits[0].currency,
            "0x20c0000000000000000000000000000000000000"
                .parse::<Address>()
                .unwrap()
        );
    }
}
