//! Data types for wallet keys.

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

fn is_false(value: &bool) -> bool {
    !*value
}

/// Wallet type: local (self-custodial EOA in OS keychain) or passkey (browser auth).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WalletType {
    #[default]
    Local,
    Passkey,
}

impl WalletType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Passkey => "passkey",
        }
    }
}

/// Cryptographic key type for key authorizations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyType {
    #[default]
    Secp256k1,
    P256,
    WebAuthn,
}

/// Token spending limit stored in keys.toml.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTokenLimit {
    /// Token contract address.
    pub currency: Address,
    /// Spending limit amount (as string to avoid precision issues).
    pub limit: String,
}

/// A single key entry.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct KeyEntry {
    /// Wallet type: "local" or "passkey".
    #[serde(default)]
    pub wallet_type: WalletType,
    /// On-chain wallet address (the fundable address).
    #[serde(default)]
    pub wallet_address: String,
    /// Chain ID this key is authorized for.
    #[serde(default)]
    pub chain_id: u64,
    /// Cryptographic key type.
    #[serde(default)]
    pub key_type: KeyType,
    /// Public address of the key (derived from the key private key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_address: Option<String>,
    /// Key private key, stored inline in keys.toml.
    /// Wrapped in [`Zeroizing`] so the secret is scrubbed from memory on drop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Zeroizing<String>>,
    /// Key authorization (RLP-encoded `SignedKeyAuthorization` hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_authorization: Option<String>,
    /// Whether this access key has been confirmed provisioned on-chain.
    #[serde(default, skip_serializing_if = "is_false")]
    pub provisioned: bool,
    /// Key expiry as unix timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<u64>,
    /// Token spending limits for this key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<StoredTokenLimit>,
}

/// TOML persistence shape for a key entry.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct StoredKeyEntry {
    #[serde(default)]
    pub wallet_type: WalletType,
    #[serde(default)]
    pub wallet_address: String,
    #[serde(default)]
    pub chain_id: u64,
    #[serde(default)]
    pub key_type: KeyType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Zeroizing<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_authorization: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub provisioned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<StoredTokenLimit>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct StoredKeystore {
    #[serde(default)]
    pub keys: Vec<StoredKeyEntry>,
}

impl From<KeyEntry> for StoredKeyEntry {
    fn from(value: KeyEntry) -> Self {
        Self {
            wallet_type: value.wallet_type,
            wallet_address: value.wallet_address,
            chain_id: value.chain_id,
            key_type: value.key_type,
            key_address: value.key_address,
            key: value.key,
            key_authorization: value.key_authorization,
            provisioned: value.provisioned,
            expiry: value.expiry,
            limits: value.limits,
        }
    }
}

impl From<StoredKeyEntry> for KeyEntry {
    fn from(value: StoredKeyEntry) -> Self {
        Self {
            wallet_type: value.wallet_type,
            wallet_address: value.wallet_address,
            chain_id: value.chain_id,
            key_type: value.key_type,
            key_address: value.key_address,
            key: value.key,
            key_authorization: value.key_authorization,
            provisioned: value.provisioned,
            expiry: value.expiry,
            limits: value.limits,
        }
    }
}

impl std::fmt::Debug for KeyEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyEntry")
            .field("wallet_type", &self.wallet_type)
            .field("wallet_address", &self.wallet_address)
            .field("chain_id", &self.chain_id)
            .field("key_type", &self.key_type)
            .field("key_address", &self.key_address)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .field("key_authorization", &self.key_authorization)
            .field("provisioned", &self.provisioned)
            .field("expiry", &self.expiry)
            .field("limits", &self.limits)
            .finish()
    }
}

impl KeyEntry {
    /// Parse and validate the wallet address field.
    #[must_use]
    pub fn wallet_address_parsed(&self) -> Option<Address> {
        (!self.wallet_address.is_empty())
            .then(|| self.wallet_address.parse().ok())
            .flatten()
    }

    /// Parse and validate the optional signer key address field.
    #[must_use]
    pub fn key_address_parsed(&self) -> Option<Address> {
        self.key_address.as_deref()?.parse().ok()
    }

    /// Canonical lowercase `0x` wallet address when valid.
    #[must_use]
    pub fn wallet_address_hex(&self) -> Option<String> {
        self.wallet_address_parsed()
            .map(|address| format!("{address:#x}"))
    }

    /// Canonical lowercase `0x` signer key address when valid.
    #[must_use]
    pub fn key_address_hex(&self) -> Option<String> {
        self.key_address_parsed()
            .map(|address| format!("{address:#x}"))
    }

    /// Set wallet address in canonical lowercase hex format.
    pub fn set_wallet_address(&mut self, address: Address) {
        self.wallet_address = format!("{address:#x}");
    }

    /// Set signer key address in canonical lowercase hex format.
    pub fn set_key_address(&mut self, address: Option<Address>) {
        self.key_address = address.map(|address| format!("{address:#x}"));
    }

    /// Validate and canonicalize persisted identity fields.
    ///
    /// Returns `false` if a non-empty wallet address or present key address
    /// cannot be parsed as an EVM address.
    pub fn normalize_identity(&mut self) -> bool {
        if !self.wallet_address.is_empty() {
            let Some(wallet) = self.wallet_address_parsed() else {
                return false;
            };
            self.set_wallet_address(wallet);
        }

        if self.key_address.is_some() {
            let Some(key) = self.key_address_parsed() else {
                return false;
            };
            self.set_key_address(Some(key));
        }

        true
    }

    /// Whether this entry has an inline private key.
    #[must_use]
    pub fn has_inline_key(&self) -> bool {
        self.key.as_ref().is_some_and(|key| !key.is_empty())
    }

    /// Whether this entry represents a direct EOA signer (wallet == signer key).
    #[must_use]
    pub fn is_direct_eoa_key(&self) -> bool {
        self.wallet_type == WalletType::Local
            && self.wallet_address_parsed().is_some()
            && self
                .key_address_parsed()
                .zip(self.wallet_address_parsed())
                .is_some_and(|(signer, wallet)| signer == wallet)
            && self.has_inline_key()
    }

    /// Compare wallet address against a parsed [`Address`].
    #[must_use]
    pub fn wallet_address_matches(&self, address: Address) -> bool {
        self.wallet_address_parsed()
            .is_some_and(|stored| stored == address)
    }

    /// Compare signer key address against a parsed [`Address`].
    #[must_use]
    pub fn key_address_matches(&self, address: Address) -> bool {
        self.key_address_parsed()
            .is_some_and(|stored| stored == address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroizing;

    #[test]
    fn key_entry_debug_redacts_key() {
        let secret = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let entry = KeyEntry {
            key: Some(Zeroizing::new(secret.to_string())),
            ..Default::default()
        };
        let debug = format!("{entry:?}");
        assert!(
            debug.contains("<redacted>"),
            "Debug output should contain <redacted>"
        );
        assert!(
            !debug.contains(secret),
            "Debug output should not contain the actual key"
        );
    }

    #[test]
    fn key_entry_serde_round_trip_all_fields() {
        let entry = KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: "0xabc".to_string(),
            chain_id: 4217,
            key_type: KeyType::P256,
            key_address: Some("0xdef".to_string()),
            key: Some(Zeroizing::new("0xsecret".to_string())),
            key_authorization: Some("0xauth".to_string()),
            provisioned: true,
            expiry: Some(1_700_000_000),
            limits: vec![StoredTokenLimit {
                currency: "0x20c000000000000000000000b9537d11c60e8b50"
                    .parse()
                    .unwrap(),
                limit: "1000".to_string(),
            }],
        };

        let toml_str = toml::to_string(&entry).unwrap();
        let deserialized: KeyEntry = toml::from_str(&toml_str).unwrap();

        assert_eq!(deserialized.wallet_type, entry.wallet_type);
        assert_eq!(deserialized.wallet_address, entry.wallet_address);
        assert_eq!(deserialized.chain_id, entry.chain_id);
        assert_eq!(deserialized.key_type, entry.key_type);
        assert_eq!(deserialized.key_address, entry.key_address);
        assert_eq!(deserialized.key.as_deref(), entry.key.as_deref());
        assert_eq!(deserialized.key_authorization, entry.key_authorization);
        assert_eq!(deserialized.provisioned, entry.provisioned);
        assert_eq!(deserialized.expiry, entry.expiry);
        assert_eq!(deserialized.limits, entry.limits);
    }

    #[test]
    fn key_entry_serde_round_trip_optional_none() {
        let entry = KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0x123".to_string(),
            chain_id: 42431,
            key_type: KeyType::Secp256k1,
            key_address: None,
            key: None,
            key_authorization: None,
            provisioned: false,
            expiry: None,
            limits: vec![],
        };

        let toml_str = toml::to_string(&entry).unwrap();
        assert!(!toml_str.contains("key_address"));
        assert!(!toml_str.contains("key ="));
        assert!(!toml_str.contains("key_authorization"));
        assert!(!toml_str.contains("provisioned"));
        assert!(!toml_str.contains("expiry"));
        assert!(!toml_str.contains("limits"));

        let deserialized: KeyEntry = toml::from_str(&toml_str).unwrap();
        assert_eq!(deserialized.key_address, None);
        assert_eq!(deserialized.key, None);
        assert_eq!(deserialized.key_authorization, None);
        assert!(!deserialized.provisioned);
        assert_eq!(deserialized.expiry, None);
        assert!(deserialized.limits.is_empty());
    }

    #[test]
    fn key_entry_parsed_addresses() {
        let entry = KeyEntry {
            wallet_address: "0x1111111111111111111111111111111111111111".to_string(),
            key_address: Some("0x2222222222222222222222222222222222222222".to_string()),
            ..Default::default()
        };
        assert_eq!(
            entry.wallet_address_parsed(),
            Some(
                "0x1111111111111111111111111111111111111111"
                    .parse()
                    .unwrap()
            )
        );
        assert_eq!(
            entry.key_address_parsed(),
            Some(
                "0x2222222222222222222222222222222222222222"
                    .parse()
                    .unwrap()
            )
        );

        assert_eq!(
            entry.wallet_address_hex().as_deref(),
            Some("0x1111111111111111111111111111111111111111")
        );
        assert_eq!(
            entry.key_address_hex().as_deref(),
            Some("0x2222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn key_entry_wallet_match_uses_typed_comparison() {
        let entry = KeyEntry {
            wallet_address: "0x1111111111111111111111111111111111111111".to_string(),
            ..Default::default()
        };

        let wallet: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let other: Address = "0x2222222222222222222222222222222222222222"
            .parse()
            .unwrap();

        assert!(entry.wallet_address_matches(wallet));
        assert!(!entry.wallet_address_matches(other));
    }

    #[test]
    fn key_entry_typed_match_helpers() {
        let entry = KeyEntry {
            wallet_address: "0x1111111111111111111111111111111111111111".to_string(),
            key_address: Some("0x2222222222222222222222222222222222222222".to_string()),
            ..Default::default()
        };

        let wallet = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let key = "0x2222222222222222222222222222222222222222"
            .parse()
            .unwrap();
        let other = "0x3333333333333333333333333333333333333333"
            .parse()
            .unwrap();

        assert!(entry.wallet_address_matches(wallet));
        assert!(entry.key_address_matches(key));
        assert!(!entry.wallet_address_matches(other));
        assert!(!entry.key_address_matches(other));
    }

    #[test]
    fn key_entry_direct_eoa_detection() {
        let entry = KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0x1111111111111111111111111111111111111111".to_string(),
            key_address: Some("0x1111111111111111111111111111111111111111".to_string()),
            key: Some(Zeroizing::new("0xkey".to_string())),
            ..Default::default()
        };
        assert!(entry.is_direct_eoa_key());

        let not_direct = KeyEntry {
            key_address: Some("0x2222222222222222222222222222222222222222".to_string()),
            ..entry
        };
        assert!(!not_direct.is_direct_eoa_key());
    }

    #[test]
    fn key_entry_normalize_identity_canonicalizes_addresses() {
        let mut entry = KeyEntry {
            wallet_address: "0x111111111111111111111111111111111111AbCd".to_string(),
            key_address: Some("0x222222222222222222222222222222222222Ef01".to_string()),
            ..Default::default()
        };

        assert!(entry.normalize_identity());
        assert_eq!(
            entry.wallet_address,
            "0x111111111111111111111111111111111111abcd"
        );
        assert_eq!(
            entry.key_address.as_deref(),
            Some("0x222222222222222222222222222222222222ef01")
        );
    }

    #[test]
    fn key_entry_normalize_identity_rejects_invalid_addresses() {
        let mut entry = KeyEntry {
            wallet_address: "not-an-address".to_string(),
            ..Default::default()
        };
        assert!(!entry.normalize_identity());

        let mut entry_with_bad_key = KeyEntry {
            wallet_address: "0x1111111111111111111111111111111111111111".to_string(),
            key_address: Some("bad-key-address".to_string()),
            ..Default::default()
        };
        assert!(!entry_with_bad_key.normalize_identity());
    }
}
