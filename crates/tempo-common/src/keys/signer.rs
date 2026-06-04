//! Signer resolution for wallet keys.
//!
//! Extends [`Keystore`] with [`signer`](Keystore::signer) —
//! resolves a network's key entry into a ready-to-use [`Signer`]
//! (private key signer + signing mode + effective `from` address).

use alloy::{
    primitives::{Address, B256},
    signers::{local::PrivateKeySigner, SignerSync},
};
use mpp::client::tempo::signing::{KeychainVersion, TempoSigningMode};
use tempo_primitives::transaction::{
    KeychainSignature, PrimitiveSignature, SignedKeyAuthorization, TempoSignature,
};

use crate::{
    error::{ConfigError, KeyError, TempoError},
    network::NetworkId,
};

use super::{authorization, KeyEntry, Keystore};

/// Parse a private key hex string into a `PrivateKeySigner`.
///
/// # Errors
///
/// Returns an error when the key is not valid hex, has the wrong length, or
/// cannot be parsed into a signer.
pub fn parse_private_key_signer(pk_str: &str) -> Result<PrivateKeySigner, TempoError> {
    let key = pk_str.trim();
    let key_hex = key.strip_prefix("0x").unwrap_or(key);
    let bytes = hex::decode(key_hex).map_err(|_| KeyError::InvalidKeyFormat)?;
    if bytes.len() != 32 {
        return Err(KeyError::InvalidKeyFormat.into());
    }
    PrivateKeySigner::from_slice(&bytes).map_err(|_| KeyError::InvalidKeyFormat.into())
}

/// A loaded wallet signer ready for transaction signing.
///
/// Bundles the private key signer, the resolved `TempoSigningMode`
/// (direct or keychain), and the effective `from` address.
///
/// The `signing_mode` always starts without `key_authorization` (optimistic:
/// assume the key is already provisioned on-chain). The stored authorization
/// is kept in `stored_key_authorization` so callers can retry with
/// [`with_key_authorization`](Signer::with_key_authorization) if the key
/// turns out not to be provisioned.
#[derive(Clone)]
pub struct Signer {
    pub signer: PrivateKeySigner,
    pub signing_mode: TempoSigningMode,
    pub from: Address,
    /// Key authorization kept aside for on-demand provisioning retries.
    /// Always `None` for direct EOA signers.
    pub stored_key_authorization: Option<Box<SignedKeyAuthorization>>,
}

impl Signer {
    fn effective_signing_hash(&self, hash: &B256) -> B256 {
        match &self.signing_mode {
            TempoSigningMode::Direct => *hash,
            TempoSigningMode::Keychain {
                wallet, version, ..
            } => match version {
                KeychainVersion::V1 => *hash,
                KeychainVersion::V2 => KeychainSignature::signing_hash(*hash, *wallet),
            },
        }
    }

    /// Returns a copy of this signer whose `signing_mode` includes the stored
    /// key authorization, so the next transaction atomically provisions the key.
    ///
    /// Returns `None` when there is no stored authorization (direct EOA signer
    /// or no authorization was configured).
    #[must_use]
    pub fn with_key_authorization(&self) -> Option<Self> {
        let auth = self.stored_key_authorization.clone()?;
        let signing_mode = match &self.signing_mode {
            TempoSigningMode::Keychain {
                wallet, version, ..
            } => TempoSigningMode::Keychain {
                wallet: *wallet,
                key_authorization: Some(auth),
                version: *version,
            },
            TempoSigningMode::Direct => return None,
        };
        Some(Self {
            signer: self.signer.clone(),
            signing_mode,
            from: self.from,
            stored_key_authorization: None,
        })
    }

    /// Whether this signer has a stored key authorization available for
    /// provisioning retries.
    #[must_use]
    pub fn has_stored_key_authorization(&self) -> bool {
        self.stored_key_authorization.is_some()
    }

    /// Sign an arbitrary digest and return the raw inner signature bytes.
    ///
    /// Direct signers return the standard 65-byte secp256k1 signature over
    /// `hash`. Keychain signers return the inner 65-byte secp256k1 signature
    /// over the effective keychain signing hash, without the outer `0x03`/`0x04`
    /// keychain envelope. This is the signature shape TIP-1020-style verifiers
    /// expect after separately resolving the authorized access key.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying signing operation fails.
    pub fn sign_hash_unwrapped_bytes(
        &self,
        hash: &B256,
        operation: &'static str,
    ) -> Result<Vec<u8>, TempoError> {
        let hash_to_sign = self.effective_signing_hash(hash);
        let signature = self
            .signer
            .sign_hash_sync(&hash_to_sign)
            .map_err(|source| {
                TempoError::from(KeyError::SigningOperationSource {
                    operation,
                    source: Box::new(source),
                })
            })?;
        Ok(signature.as_bytes().to_vec())
    }

    /// Sign an arbitrary digest and return the raw inner signature as a
    /// 0x-prefixed hex string.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying signing operation fails.
    pub fn sign_hash_unwrapped_hex(
        &self,
        hash: &B256,
        operation: &'static str,
    ) -> Result<String, TempoError> {
        let bytes = self.sign_hash_unwrapped_bytes(hash, operation)?;
        Ok(format!("0x{}", hex::encode(bytes)))
    }

    /// Sign an arbitrary digest and return the serialized Tempo signature bytes.
    ///
    /// Direct signers return a raw 65-byte secp256k1 signature. Keychain
    /// signers return a Tempo keychain envelope (type 0x03/0x04) wrapping the
    /// inner secp256k1 signature for the configured wallet address.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying signing operation fails.
    pub fn sign_hash_bytes(
        &self,
        hash: &B256,
        operation: &'static str,
    ) -> Result<Vec<u8>, TempoError> {
        let hash_to_sign = self.effective_signing_hash(hash);

        let inner_signature = self
            .signer
            .sign_hash_sync(&hash_to_sign)
            .map_err(|source| {
                TempoError::from(KeyError::SigningOperationSource {
                    operation,
                    source: Box::new(source),
                })
            })?;

        let signature = match &self.signing_mode {
            TempoSigningMode::Direct => {
                TempoSignature::Primitive(PrimitiveSignature::Secp256k1(inner_signature))
            }
            TempoSigningMode::Keychain {
                wallet, version, ..
            } => {
                let primitive = PrimitiveSignature::Secp256k1(inner_signature);
                let keychain = match version {
                    KeychainVersion::V1 => KeychainSignature::new_v1(*wallet, primitive),
                    KeychainVersion::V2 => KeychainSignature::new(*wallet, primitive),
                };
                TempoSignature::Keychain(keychain)
            }
        };

        Ok(signature.to_bytes().to_vec())
    }

    /// Sign an arbitrary digest and return the serialized Tempo signature as a
    /// 0x-prefixed hex string.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying signing operation fails.
    pub fn sign_hash_hex(
        &self,
        hash: &B256,
        operation: &'static str,
    ) -> Result<String, TempoError> {
        let bytes = self.sign_hash_bytes(hash, operation)?;
        Ok(format!("0x{}", hex::encode(bytes)))
    }
}

fn signer_from_key_entry(key_entry: &KeyEntry) -> Result<Signer, TempoError> {
    let pk = key_entry
        .key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TempoError::from(ConfigError::Missing("No key configured.".to_string())))?;
    let signer = parse_private_key_signer(pk)?;

    let wallet_address: Address = key_entry.wallet_address_parsed().ok_or_else(|| {
        TempoError::from(ConfigError::InvalidAddress {
            context: "wallet",
            value: key_entry.wallet_address.clone(),
        })
    })?;

    let (signing_mode, stored_key_authorization) = if wallet_address == signer.address() {
        (TempoSigningMode::Direct, None)
    } else {
        // Decode the local key authorization but always start optimistically
        // without it (assume key is already provisioned on-chain).
        // The authorization is stored separately so callers can retry with
        // `with_key_authorization()` if the key turns out not to be provisioned.
        let local_auth = key_entry
            .key_authorization
            .as_deref()
            .and_then(authorization::decode)
            .map(Box::new);

        (
            TempoSigningMode::Keychain {
                wallet: wallet_address,
                key_authorization: None,
                version: KeychainVersion::V2,
            },
            local_auth,
        )
    };

    let from = signing_mode.from_address(signer.address());

    Ok(Signer {
        signer,
        signing_mode,
        from,
        stored_key_authorization,
    })
}

impl Keystore {
    /// Resolve the wallet signer for a network.
    ///
    /// Looks up the key entry for the network, parses the private key,
    /// resolves the signing mode (direct EOA or keychain with optional
    /// key authorization), and returns a ready-to-use [`Signer`].
    ///
    /// # Errors
    ///
    /// Returns an error when no key is configured for the network, stored
    /// addresses are malformed, or signer parsing fails.
    pub fn signer(&self, network: NetworkId) -> Result<Signer, TempoError> {
        let key_entry = self.key_for_network(network).ok_or_else(|| {
            TempoError::from(ConfigError::Missing(format!(
                "No key configured for network '{}'.",
                network.as_str()
            )))
        })?;

        signer_from_key_entry(key_entry)
    }

    /// Resolve the signer for a specific wallet on a network.
    ///
    /// Matches an exact wallet+network entry first, then falls back to a
    /// direct EOA entry for the same wallet because those keys can sign on any
    /// network.
    ///
    /// # Errors
    ///
    /// Returns an error when no key is configured for `wallet_address` on
    /// `network`, stored addresses are malformed, or signer parsing fails.
    pub fn signer_for_wallet_address(
        &self,
        wallet_address: Address,
        network: NetworkId,
    ) -> Result<Signer, TempoError> {
        let key_entry = self
            .key_for_wallet_address_and_network(wallet_address, network)
            .or_else(|| {
                self.keys.iter().find(|key| {
                    key.wallet_address_matches(wallet_address) && key.is_direct_eoa_key()
                })
            })
            .ok_or_else(|| {
                TempoError::from(ConfigError::Missing(format!(
                    "No key configured for wallet '{wallet_address:#x}' on network '{}'.",
                    network.as_str()
                )))
            })?;

        signer_from_key_entry(key_entry)
    }

    /// Resolve a signer for either a wallet address or an access-key address.
    ///
    /// If `address` matches a stored wallet address, this behaves like
    /// [`signer_for_wallet_address`](Self::signer_for_wallet_address). If it
    /// matches a stored `key_address`, it returns a direct signer for that key
    /// identity so commands can operate on balances that are intentionally held
    /// on the access key itself.
    pub fn signer_for_identity_address(
        &self,
        address: Address,
        network: NetworkId,
    ) -> Result<Signer, TempoError> {
        if let Ok(signer) = self.signer_for_wallet_address(address, network) {
            return Ok(signer);
        }

        let chain_id = network.chain_id();
        let key_entry = self
            .keys
            .iter()
            .find(|key| key.key_address_matches(address) && key.chain_id == chain_id)
            .ok_or_else(|| {
                TempoError::from(ConfigError::Missing(format!(
                    "No key configured for identity '{address:#x}' on network '{}'.",
                    network.as_str()
                )))
            })?;

        let mut direct_entry = key_entry.clone();
        direct_entry.set_wallet_address(address);
        direct_entry.set_key_address(Some(address));
        direct_entry.key_authorization = None;

        signer_from_key_entry(&direct_entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::keccak256;
    use zeroize::Zeroizing;

    use crate::keys::KeyEntry;

    const TEST_PRIVATE_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const TEST_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    #[test]
    fn test_signer_direct_when_wallet_equals_key() {
        let keys = Keystore::from_private_key(TEST_PRIVATE_KEY).unwrap();
        let signer = keys.signer(NetworkId::Tempo).unwrap();
        assert!(matches!(signer.signing_mode, TempoSigningMode::Direct));
        assert_eq!(signer.from, signer.signer.address());
    }

    #[test]
    fn test_signer_keychain_when_wallet_differs_from_key() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_address: "0x70997970C51812dc3A010C7d01b50e0d17dc79C8".to_string(),
            key_address: Some(TEST_ADDRESS.to_string()),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            chain_id: 4217,
            ..Default::default()
        });
        let signer = keys.signer(NetworkId::Tempo).unwrap();
        match signer.signing_mode {
            TempoSigningMode::Keychain { wallet, .. } => {
                assert_eq!(
                    wallet,
                    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
                        .parse::<Address>()
                        .unwrap()
                );
            }
            TempoSigningMode::Direct => panic!("expected Keychain mode"),
        }
    }

    #[test]
    fn test_signer_for_identity_address_uses_direct_signer_for_key_address() {
        let mut keys = Keystore::default();
        let key_address: Address = TEST_ADDRESS.parse().unwrap();
        keys.keys.push(KeyEntry {
            wallet_address: "0x70997970C51812dc3A010C7d01b50e0d17dc79C8".to_string(),
            key_address: Some(TEST_ADDRESS.to_string()),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            chain_id: NetworkId::TempoModerato.chain_id(),
            ..Default::default()
        });

        let signer = keys
            .signer_for_identity_address(key_address, NetworkId::TempoModerato)
            .unwrap();

        assert!(matches!(signer.signing_mode, TempoSigningMode::Direct));
        assert_eq!(signer.from, key_address);
        assert_eq!(signer.signer.address(), key_address);
    }

    #[test]
    fn test_signer_for_wallet_address_selects_requested_wallet_key() {
        const SECOND_PRIVATE_KEY: &str =
            "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

        let requested_wallet: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let other_wallet: Address = "0x2222222222222222222222222222222222222222"
            .parse()
            .unwrap();
        let other_signer_address = parse_private_key_signer(SECOND_PRIVATE_KEY)
            .unwrap()
            .address();

        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_address: format!("{other_wallet:#x}"),
            key_address: Some(format!("{other_signer_address:#x}")),
            key: Some(Zeroizing::new(SECOND_PRIVATE_KEY.to_string())),
            chain_id: NetworkId::TempoModerato.chain_id(),
            ..Default::default()
        });
        keys.keys.push(KeyEntry {
            wallet_address: format!("{requested_wallet:#x}"),
            key_address: Some(TEST_ADDRESS.to_string()),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            chain_id: NetworkId::TempoModerato.chain_id(),
            ..Default::default()
        });

        let default_signer = keys.signer(NetworkId::TempoModerato).unwrap();
        assert_eq!(default_signer.signer.address(), other_signer_address);

        let signer = keys
            .signer_for_wallet_address(requested_wallet, NetworkId::TempoModerato)
            .unwrap();

        assert_eq!(
            signer.signer.address(),
            TEST_ADDRESS.parse::<Address>().unwrap()
        );
        match signer.signing_mode {
            TempoSigningMode::Keychain { wallet, .. } => {
                assert_eq!(wallet, requested_wallet);
            }
            TempoSigningMode::Direct => panic!("expected Keychain mode"),
        }
    }

    #[test]
    fn test_signer_for_wallet_address_direct_eoa_falls_back_across_networks() {
        let keys = Keystore::from_private_key(TEST_PRIVATE_KEY).unwrap();
        let wallet_address = keys.wallet_address_parsed().unwrap();

        let signer = keys
            .signer_for_wallet_address(wallet_address, NetworkId::TempoModerato)
            .unwrap();

        assert!(matches!(signer.signing_mode, TempoSigningMode::Direct));
        assert_eq!(signer.signer.address(), wallet_address);
    }

    #[test]
    fn test_signer_keychain_always_omits_auth_from_signing_mode() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_address: "0x70997970C51812dc3A010C7d01b50e0d17dc79C8".to_string(),
            key_address: Some(TEST_ADDRESS.to_string()),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            key_authorization: Some("deadbeef".to_string()),
            chain_id: 4217,
            ..Default::default()
        });
        let signer = keys.signer(NetworkId::Tempo).unwrap();
        // signing_mode always starts without key_authorization (optimistic)
        match &signer.signing_mode {
            TempoSigningMode::Keychain {
                key_authorization, ..
            } => {
                assert!(key_authorization.is_none());
            }
            TempoSigningMode::Direct => panic!("expected Keychain mode"),
        }
        // The auth is not available via with_key_authorization because
        // "deadbeef" doesn't decode to a valid SignedKeyAuthorization.
        assert!(!signer.has_stored_key_authorization());
    }

    /// Regression test for the provisioned-flag desync bug.
    ///
    /// Previously, when keys.toml had `provisioned = true` but the key wasn't
    /// actually registered on-chain, the signer dropped the key_authorization
    /// entirely — making auto-provisioning impossible without manually editing
    /// keys.toml to set `provisioned = false`.
    ///
    /// The fix: always start optimistically without auth in signing_mode, but
    /// keep valid auth in `stored_key_authorization` for on-demand retry via
    /// `with_key_authorization()`.
    #[test]
    fn test_signer_keychain_preserves_valid_auth_for_retry() {
        // Create a valid key authorization via the authorization::sign helper.
        // This simulates the state after `tempo wallet login` creates a key
        // authorization for a freshly provisioned access key.
        let wallet_signer = parse_private_key_signer(
            "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
        )
        .unwrap();
        let access_signer = parse_private_key_signer(TEST_PRIVATE_KEY).unwrap();
        let auth = authorization::sign(&wallet_signer, &access_signer, 4217).unwrap();

        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_address: format!("{:#x}", wallet_signer.address()),
            key_address: Some(TEST_ADDRESS.to_string()),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            key_authorization: Some(auth.hex),
            chain_id: 4217,
            ..Default::default()
        });

        let signer = keys.signer(NetworkId::Tempo).unwrap();

        // signing_mode starts WITHOUT key_authorization (optimistic path)
        match &signer.signing_mode {
            TempoSigningMode::Keychain {
                key_authorization, ..
            } => {
                assert!(
                    key_authorization.is_none(),
                    "signing_mode should start without key_authorization (optimistic)"
                );
            }
            TempoSigningMode::Direct => panic!("expected Keychain mode"),
        }

        // The valid auth MUST be stored for retry — this is the fix.
        // On the old code with `provisioned = true`, the auth was dropped
        // entirely and there was no stored_key_authorization mechanism.
        assert!(
            signer.has_stored_key_authorization(),
            "valid key_authorization must be stored for provisioning retries"
        );

        // Retry path: with_key_authorization() attaches the auth
        let provisioning_signer = signer
            .with_key_authorization()
            .expect("should produce a provisioning signer");
        assert!(
            provisioning_signer
                .signing_mode
                .key_authorization()
                .is_some(),
            "retry signer must include key_authorization for on-chain provisioning"
        );
    }

    #[test]
    fn test_signer_direct_has_no_stored_auth() {
        let keys = Keystore::from_private_key(TEST_PRIVATE_KEY).unwrap();
        let signer = keys.signer(NetworkId::Tempo).unwrap();
        assert!(!signer.has_stored_key_authorization());
        assert!(signer.with_key_authorization().is_none());
    }

    #[test]
    fn test_sign_hash_hex_direct_returns_raw_signature() {
        let keys = Keystore::from_private_key(TEST_PRIVATE_KEY).unwrap();
        let signer = keys.signer(NetworkId::Tempo).unwrap();
        let hash = keccak256(b"coinflow-direct");

        let signature_hex = signer
            .sign_hash_hex(&hash, "sign direct test hash")
            .unwrap();
        let bytes = hex::decode(signature_hex.trim_start_matches("0x")).unwrap();
        let signature = TempoSignature::from_bytes(&bytes).unwrap();

        assert!(matches!(signature, TempoSignature::Primitive(_)));
        assert_eq!(
            signature.recover_signer(&hash).unwrap(),
            signer.signer.address()
        );
    }

    #[test]
    fn test_sign_hash_hex_keychain_returns_v2_envelope() {
        let mut keys = Keystore::default();
        let wallet_address: Address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
            .parse()
            .unwrap();
        keys.keys.push(KeyEntry {
            wallet_address: format!("{wallet_address:#x}"),
            key_address: Some(TEST_ADDRESS.to_string()),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            chain_id: 4217,
            ..Default::default()
        });
        let signer = keys.signer(NetworkId::Tempo).unwrap();
        let hash = keccak256(b"coinflow-keychain");

        let signature_hex = signer
            .sign_hash_hex(&hash, "sign keychain test hash")
            .unwrap();
        let bytes = hex::decode(signature_hex.trim_start_matches("0x")).unwrap();
        let signature = TempoSignature::from_bytes(&bytes).unwrap();
        let keychain = signature
            .as_keychain()
            .expect("expected keychain signature");

        assert_eq!(bytes[0], 0x04, "expected V2 keychain type byte");
        assert_eq!(keychain.user_address, wallet_address);
        assert_eq!(signature.recover_signer(&hash).unwrap(), wallet_address);
        assert_eq!(keychain.key_id(&hash).unwrap(), signer.signer.address());
    }

    #[test]
    fn test_sign_hash_unwrapped_hex_keychain_returns_raw_inner_signature() {
        let mut keys = Keystore::default();
        let wallet_address: Address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
            .parse()
            .unwrap();
        keys.keys.push(KeyEntry {
            wallet_address: format!("{wallet_address:#x}"),
            key_address: Some(TEST_ADDRESS.to_string()),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            chain_id: 4217,
            ..Default::default()
        });
        let signer = keys.signer(NetworkId::Tempo).unwrap();
        let hash = keccak256(b"coinflow-keychain-tip1020");

        let signature_hex = signer
            .sign_hash_unwrapped_hex(&hash, "sign keychain test hash for tip-1020")
            .unwrap();
        let bytes = hex::decode(signature_hex.trim_start_matches("0x")).unwrap();
        let signature = TempoSignature::from_bytes(&bytes).unwrap();

        assert_eq!(bytes.len(), 65, "tip-1020 expects the raw inner signature");
        assert!(matches!(signature, TempoSignature::Primitive(_)));
        let effective_hash = KeychainSignature::signing_hash(hash, wallet_address);
        assert_eq!(
            signature.recover_signer(&effective_hash).unwrap(),
            signer.signer.address()
        );
    }

    #[test]
    fn test_sign_hash_unwrapped_hex_direct_matches_wrapped_output() {
        let keys = Keystore::from_private_key(TEST_PRIVATE_KEY).unwrap();
        let signer = keys.signer(NetworkId::Tempo).unwrap();
        let hash = keccak256(b"coinflow-direct-tip1020");

        let wrapped = signer.sign_hash_hex(&hash, "sign direct hash").unwrap();
        let unwrapped = signer
            .sign_hash_unwrapped_hex(&hash, "sign direct hash for tip-1020")
            .unwrap();

        assert_eq!(wrapped, unwrapped);
    }

    #[test]
    fn test_signer_no_key_for_network() {
        let keys = Keystore::default();
        assert!(keys.signer(NetworkId::Tempo).is_err());
    }

    #[test]
    fn test_signer_empty_key_rejected() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_address: TEST_ADDRESS.to_string(),
            key: Some(Zeroizing::new(String::new())),
            chain_id: 4217,
            ..Default::default()
        });
        assert!(keys.signer(NetworkId::Tempo).is_err());
    }

    #[test]
    fn test_parse_private_key_signer_valid() {
        let signer = parse_private_key_signer(TEST_PRIVATE_KEY).unwrap();
        assert_eq!(
            format!("{}", signer.address()).to_lowercase(),
            TEST_ADDRESS.to_lowercase()
        );
    }

    #[test]
    fn test_parse_private_key_signer_no_prefix() {
        let no_prefix = TEST_PRIVATE_KEY.strip_prefix("0x").unwrap();
        let signer = parse_private_key_signer(no_prefix).unwrap();
        assert_eq!(
            format!("{}", signer.address()).to_lowercase(),
            TEST_ADDRESS.to_lowercase()
        );
    }

    #[test]
    fn test_parse_private_key_signer_invalid_hex() {
        assert!(parse_private_key_signer("not-hex").is_err());
    }

    #[test]
    fn test_parse_private_key_signer_wrong_length() {
        assert!(parse_private_key_signer("0xdeadbeef").is_err());
    }
}
