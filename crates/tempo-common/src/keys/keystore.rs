//! Keystore query, selection, and mutation logic.

use alloy::primitives::Address;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

use crate::{
    error::{ConfigError, TempoError},
    network::NetworkId,
};

use super::model::{KeyEntry, StoredKeystore, WalletType};

/// Wallet keys stored in keys.toml.
///
/// Supports multiple key entries via `[[keys]]` array of tables.
/// Key selection is deterministic: passkey > first key with key > first key.
#[derive(Debug, Clone, Default)]
pub struct Keystore {
    pub(crate) keys: Vec<KeyEntry>,

    /// Whether this keystore was built from an ephemeral `--private-key`
    /// override. Ephemeral keystores are never written to disk.
    pub ephemeral: bool,
}

impl Serialize for Keystore {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let stored = StoredKeystore {
            keys: self.keys.iter().cloned().map(Into::into).collect(),
        };
        stored.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Keystore {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let stored = StoredKeystore::deserialize(deserializer)?;
        Ok(Self {
            keys: stored.keys.into_iter().map(Into::into).collect(),
            ephemeral: false,
        })
    }
}

impl Keystore {
    /// Iterate over key entries.
    pub fn iter(&self) -> impl Iterator<Item = &KeyEntry> {
        self.keys.iter()
    }

    /// Number of key entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the keystore has no keys.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Create ephemeral keys from a raw private key (for `--private-key`).
    ///
    /// Derives the address from the key and creates a single-account
    /// key set with an inline key. Not written to disk.
    ///
    /// # Errors
    ///
    /// Returns an error when the provided private key cannot be parsed.
    pub fn from_private_key(key: &str) -> Result<Self, TempoError> {
        let signer = super::parse_private_key_signer(key)?;
        let mut key_entry = KeyEntry {
            key: Some(Zeroizing::new(key.to_string())),
            ..Default::default()
        };
        key_entry.set_wallet_address(signer.address());
        key_entry.set_key_address(Some(signer.address()));
        Ok(Self {
            keys: vec![key_entry],
            ephemeral: true,
        })
    }

    /// Get the primary key entry.
    ///
    /// Deterministic selection: passkey > first key with a signing key > first entry.
    #[must_use]
    pub fn primary_key(&self) -> Option<&KeyEntry> {
        if let Some(entry) = self
            .keys
            .iter()
            .find(|k| k.wallet_type == WalletType::Passkey)
        {
            return Some(entry);
        }
        if let Some(entry) = self.keys.iter().find(|entry| entry.has_inline_key()) {
            return Some(entry);
        }
        self.keys.first()
    }

    /// Check if a wallet is configured.
    ///
    /// Returns `true` when the primary key has a wallet address AND
    /// an inline `key`.
    #[must_use]
    pub fn has_wallet(&self) -> bool {
        self.primary_key()
            .is_some_and(|entry| entry.wallet_address_parsed().is_some() && entry.has_inline_key())
    }

    /// Check if a wallet is connected with a key for the given network.
    #[must_use]
    pub fn has_key_for_network(&self, network: NetworkId) -> bool {
        self.has_wallet() && self.keys.iter().any(|k| k.chain_id == network.chain_id())
    }

    /// Check if a wallet is connected with a key for the given network AND currency.
    ///
    /// Used to filter multi-challenge 402 responses down to challenges the wallet
    /// can actually satisfy. Returns `true` when:
    ///
    /// - the keystore is empty (nothing to filter on; preserves prior behavior for
    ///   unauthenticated flows like `--dry-run` without `tempo wallet login`), or
    /// - the keystore is ephemeral (a raw `--private-key` can sign anything), or
    /// - any key in the store covers the requested `network` (matching
    ///   `chain_id`, or a direct-EOA key that can sign on any network), AND
    /// - that key's `limits` either include `currency` (address compare) or
    ///   are empty (unlimited key).
    ///
    /// `currency` is the on-wire token address from the challenge (`0x…`). If it
    /// doesn't parse as an `Address`, currency-level filtering is skipped and
    /// network-only matching is used.
    ///
    /// The network match mirrors [`Self::key_for_network`] so this predicate
    /// agrees with later signer resolution.
    #[must_use]
    pub fn has_key_for_network_and_currency(&self, network: NetworkId, currency: &str) -> bool {
        if self.is_empty() || self.ephemeral {
            return true;
        }
        if !self.has_wallet() {
            return false;
        }
        let currency_addr = currency.parse::<Address>().ok();
        self.keys.iter().any(|k| {
            let network_matches = k.chain_id == network.chain_id() || k.is_direct_eoa_key();
            let currency_matches = currency_addr.is_none_or(|addr| {
                k.limits.is_empty() || k.limits.iter().any(|l| l.currency == addr)
            });
            network_matches && currency_matches
        })
    }

    /// Ensure a wallet with a key for the given network is available.
    ///
    /// Returns an error with a helpful message if no wallet or key is configured.
    ///
    /// # Errors
    ///
    /// Returns an error when no usable wallet/key exists for `network`.
    pub fn ensure_key_for_network(&self, network: NetworkId) -> Result<(), TempoError> {
        let setup_cmd = "tempo wallet login";

        if !self.has_key_for_network(network) {
            let msg = if self.has_wallet() {
                format!(
                    "No key configured for network '{}'. Run '{setup_cmd}'.",
                    network.as_str()
                )
            } else {
                format!("No wallet configured. Run '{setup_cmd}'.")
            };
            return Err(ConfigError::Missing(msg).into());
        }

        Ok(())
    }

    /// Get the wallet address of the primary key.
    #[must_use]
    pub fn wallet_address(&self) -> &str {
        self.primary_key().map_or("", |a| a.wallet_address.as_str())
    }

    /// Parse the wallet address as an [`Address`], returning `None` if no wallet is configured
    /// or the address is invalid.
    pub fn wallet_address_parsed(&self) -> Option<Address> {
        self.primary_key().and_then(KeyEntry::wallet_address_parsed)
    }

    /// Canonical lowercase `0x` wallet address of the primary key when valid.
    pub fn wallet_address_hex(&self) -> Option<String> {
        self.primary_key().and_then(KeyEntry::wallet_address_hex)
    }

    /// Find the key for a given network.
    ///
    /// Matches on `chain_id`, then falls back to direct EOA keys
    /// (wallet == signer) which work on any network.
    #[must_use]
    pub fn key_for_network(&self, network: NetworkId) -> Option<&KeyEntry> {
        let chain_id = network.chain_id();
        // Try exact chain_id match first
        if let Some(entry) = self.keys.iter().find(|k| k.chain_id == chain_id) {
            return Some(entry);
        }
        // Direct EOA keys (wallet == signer) work on any network
        self.keys.iter().find(|k| k.is_direct_eoa_key())
    }

    /// Find the key for a specific parsed wallet address on a given network.
    #[must_use]
    pub fn key_for_wallet_address_and_network(
        &self,
        wallet_address: Address,
        network: NetworkId,
    ) -> Option<&KeyEntry> {
        let chain_id = network.chain_id();
        self.keys
            .iter()
            .find(|k| k.wallet_address_matches(wallet_address) && k.chain_id == chain_id)
    }

    /// Find the first passkey wallet entry, if one exists.
    #[must_use]
    pub fn find_passkey_wallet(&self) -> Option<&KeyEntry> {
        self.keys
            .iter()
            .find(|k| k.wallet_type == WalletType::Passkey)
    }

    /// Delete all passkey entries for a parsed wallet address.
    ///
    /// # Errors
    ///
    /// Returns an error when no passkey entries match `wallet_address`.
    pub fn delete_passkey_wallet_address(
        &mut self,
        wallet_address: Address,
    ) -> Result<(), TempoError> {
        let before = self.keys.len();
        self.keys.retain(|k| {
            !(k.wallet_type == WalletType::Passkey && k.wallet_address_matches(wallet_address))
        });
        if self.keys.len() == before {
            return Err(ConfigError::Missing(format!(
                "No passkey wallet found for '{wallet_address:#x}'."
            ))
            .into());
        }
        Ok(())
    }

    /// Find or create an entry by parsed wallet address and chain ID.
    pub fn upsert_by_wallet_address_and_chain(
        &mut self,
        wallet_address: Address,
        chain_id: u64,
    ) -> &mut KeyEntry {
        let idx = self
            .keys
            .iter()
            .position(|k| k.wallet_address_matches(wallet_address) && k.chain_id == chain_id);
        if let Some(i) = idx {
            &mut self.keys[i]
        } else {
            let mut entry = KeyEntry {
                chain_id,
                ..Default::default()
            };
            entry.set_wallet_address(wallet_address);
            self.keys.push(entry);
            let last = self.keys.len() - 1;
            &mut self.keys[last]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PRIVATE_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const TEST_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    /// Helper to create a Keystore with a single passkey entry.
    fn make_keys(address: &str, key: Option<&str>) -> Keystore {
        let mut keys = Keystore::default();
        let key_entry = KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: address.to_string(),
            key: key
                .filter(|s| !s.is_empty())
                .map(|s| Zeroizing::new(s.to_string())),
            ..Default::default()
        };
        keys.keys.push(key_entry);
        keys
    }

    #[test]
    fn test_default_keys() {
        let keys = Keystore::default();
        assert!(!keys.has_wallet());
        assert!(keys.primary_key().is_none());
        assert!(keys.keys.is_empty());
    }

    #[test]
    fn test_has_wallet() {
        // No keys at all
        let keys = Keystore::default();
        assert!(!keys.has_wallet());

        // wallet_address alone is not enough
        let keys = make_keys(TEST_ADDRESS, None);
        assert!(!keys.has_wallet());

        // needs wallet_address + key
        let keys = make_keys(TEST_ADDRESS, Some(TEST_PRIVATE_KEY));
        assert!(keys.has_wallet());

        // empty key doesn't count
        let keys = make_keys(TEST_ADDRESS, Some(""));
        assert!(!keys.has_wallet());

        // malformed wallet addresses are not considered configured
        let keys = make_keys("not-an-address", Some(TEST_PRIVATE_KEY));
        assert!(!keys.has_wallet());
    }

    #[test]
    fn test_wallet_address_hex_is_canonical() {
        let keys = make_keys(
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
            Some(TEST_PRIVATE_KEY),
        );
        assert_eq!(
            keys.wallet_address_hex().as_deref(),
            Some("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266")
        );
    }

    #[test]
    fn test_serialization_with_key() {
        let mut keys = Keystore::default();
        let key_entry = KeyEntry {
            wallet_address: TEST_ADDRESS.to_string(),
            key_address: Some("0xsigner".to_string()),
            key: Some(Zeroizing::new("0xaccesskey".to_string())),
            key_authorization: Some("auth123".to_string()),
            chain_id: 4217,
            ..Default::default()
        };
        keys.keys.push(key_entry);

        let toml_str = toml::to_string_pretty(&keys).unwrap();
        assert!(toml_str.contains("key_address = \"0xsigner\""));
        assert!(toml_str.contains("key = \"0xaccesskey\""));
        assert!(!toml_str.contains("private_key"));

        let parsed: Keystore = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.wallet_address(), TEST_ADDRESS);
        assert!(parsed.has_wallet());
    }

    #[test]
    fn test_new_format_loads_correctly() {
        let toml_str = r#"
[[keys]]
wallet_address = "0x1111111111111111111111111111111111111111"
chain_id = 4217
key_address = "0xsigner"
key = "0xaccesskey"
key_authorization = "auth123"
"#;
        let keys: Keystore = toml::from_str(toml_str).unwrap();
        assert_eq!(
            keys.wallet_address(),
            "0x1111111111111111111111111111111111111111"
        );
        assert!(keys.has_wallet());
    }

    #[test]
    fn test_wallet_address_only_not_enough() {
        let toml_str = r#"
[[keys]]
wallet_address = "0xtest"
"#;
        let keys: Keystore = toml::from_str(toml_str).unwrap();
        assert_eq!(keys.wallet_address(), "0xtest");
        assert!(!keys.has_wallet());
    }

    #[test]
    fn test_insert_passkey_entry() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: "0x1111111111111111111111111111111111111111".to_string(),
            key_address: Some("0xsigner1".to_string()),
            key: Some(Zeroizing::new("0xaccesskey1".to_string())),
            key_authorization: Some("auth".to_string()),
            ..Default::default()
        });
        assert!(keys.primary_key().is_some());
        assert_eq!(
            keys.wallet_address(),
            "0x1111111111111111111111111111111111111111"
        );
        assert!(keys.has_wallet());
        let key_entry = keys.primary_key().unwrap();
        assert_eq!(key_entry.key_address, Some("0xsigner1".to_string()));
    }

    #[test]
    fn test_multiple_keys() {
        let toml_str = r#"
[[keys]]
wallet_address = "0xAAA"
chain_id = 4217
key_address = "0xsigner1"

[[keys]]
wallet_address = "0xBBB"
chain_id = 42431
key_address = "0xsigner2"
"#;
        let keys: Keystore = toml::from_str(toml_str).unwrap();
        assert_eq!(keys.wallet_address(), "0xAAA");
    }

    #[test]
    fn test_delete_passkey() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xAAA".to_string(),
            key: Some(Zeroizing::new("0xaccess".to_string())),
            ..Default::default()
        });
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: "0xBBB".to_string(),
            key: Some(Zeroizing::new("0xaccess".to_string())),
            ..Default::default()
        });

        let passkey_wallet: Address = "0xbbb0000000000000000000000000000000000000"
            .parse()
            .unwrap();
        keys.keys[1].wallet_address = format!("{passkey_wallet:#x}");

        keys.delete_passkey_wallet_address(passkey_wallet).unwrap();
        assert_eq!(keys.keys.len(), 1);
        assert_eq!(keys.primary_key().unwrap().wallet_address, "0xAAA");
    }

    #[test]
    fn test_delete_passkey_wallet_address() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: "0x1111111111111111111111111111111111111111".to_string(),
            ..Default::default()
        });
        let wallet: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();

        keys.delete_passkey_wallet_address(wallet).unwrap();
        assert!(keys.keys.is_empty());
    }

    #[test]
    fn test_from_private_key() {
        let keys = Keystore::from_private_key(TEST_PRIVATE_KEY).unwrap();
        assert_eq!(
            keys.wallet_address().to_lowercase(),
            TEST_ADDRESS.to_lowercase()
        );
        assert!(keys.has_wallet());
    }

    #[test]
    fn test_from_private_key_invalid() {
        assert!(Keystore::from_private_key("not-a-key").is_err());
    }

    #[test]
    fn test_primary_key_resolves_first() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_address: "0xtest".to_string(),
            ..Default::default()
        });
        assert_eq!(keys.primary_key().unwrap().wallet_address, "0xtest");
    }

    #[test]
    fn test_upsert_by_wallet_and_chain_creates_new() {
        let mut keys = Keystore::default();
        let wallet: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let entry = keys.upsert_by_wallet_address_and_chain(wallet, 4217);
        entry.wallet_type = WalletType::Passkey;
        entry.key_address = Some("0xsigner1".to_string());
        entry.key = Some(Zeroizing::new("0xaccesskey1".to_string()));

        assert_eq!(keys.keys.len(), 1);
        assert_eq!(keys.wallet_address(), format!("{wallet:#x}"));
    }

    #[test]
    fn test_upsert_by_wallet_and_chain_updates_existing() {
        let mut keys = Keystore::default();
        let wallet: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: format!("{wallet:#x}"),
            chain_id: 4217,
            key_address: Some("0xsigner1".to_string()),
            key: Some(Zeroizing::new("0xaccesskey1".to_string())),
            ..Default::default()
        });

        let entry = keys.upsert_by_wallet_address_and_chain(wallet, 4217);
        entry.key_address = Some("0xsigner2".to_string());
        entry.key = Some(Zeroizing::new("0xaccesskey2".to_string()));

        assert_eq!(keys.keys.len(), 1);
        let key_entry = keys.primary_key().unwrap();
        assert_eq!(key_entry.key_address, Some("0xsigner2".to_string()));
    }

    #[test]
    fn test_upsert_by_wallet_address_and_chain_updates_existing() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_address: "0x1111111111111111111111111111111111111111".to_string(),
            chain_id: 4217,
            ..Default::default()
        });
        let wallet: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();

        let entry = keys.upsert_by_wallet_address_and_chain(wallet, 4217);
        entry.key_address = Some("0xnewsigner".to_string());

        assert_eq!(keys.keys.len(), 1);
        assert_eq!(keys.keys[0].key_address.as_deref(), Some("0xnewsigner"));
    }

    #[test]
    fn test_upsert_by_wallet_and_chain_different_chains() {
        let mut keys = Keystore::default();
        let wallet: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let entry = keys.upsert_by_wallet_address_and_chain(wallet, 4217);
        entry.wallet_type = WalletType::Passkey;
        entry.key_address = Some("0xsigner1".to_string());
        entry.key = Some(Zeroizing::new("0xkey1".to_string()));

        let entry2 = keys.upsert_by_wallet_address_and_chain(wallet, 42431);
        entry2.wallet_type = WalletType::Passkey;
        entry2.key_address = Some("0xsigner2".to_string());
        entry2.key = Some(Zeroizing::new("0xkey2".to_string()));

        assert_eq!(keys.keys.len(), 2);
        assert_eq!(keys.keys[0].chain_id, 4217);
        assert_eq!(keys.keys[1].chain_id, 42431);
    }

    #[test]
    fn test_find_passkey() {
        let mut keys = Keystore::default();
        assert!(keys.find_passkey_wallet().is_none());

        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: "0xABC".to_string(),
            ..Default::default()
        });
        assert!(keys.find_passkey_wallet().is_some());
        assert_eq!(keys.find_passkey_wallet().unwrap().wallet_address, "0xABC");
    }

    #[test]
    fn test_key_for_wallet_address_and_network() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_address: "0x1111111111111111111111111111111111111111".to_string(),
            chain_id: 4217,
            ..Default::default()
        });
        let wallet: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();

        assert!(keys
            .key_for_wallet_address_and_network(wallet, NetworkId::Tempo)
            .is_some());
        assert!(keys
            .key_for_wallet_address_and_network(wallet, NetworkId::TempoModerato)
            .is_none());
    }

    #[test]
    fn test_key_for_network_passkey_no_cross_network_fallback() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: "0xABC".to_string(),
            key: Some(Zeroizing::new("0xaccess".to_string())),
            chain_id: 4217,
            ..Default::default()
        });
        assert!(keys.key_for_network(NetworkId::Tempo).is_some());
        // Passkey on Tempo must NOT match TempoModerato
        assert!(keys.key_for_network(NetworkId::TempoModerato).is_none());
    }

    #[test]
    fn test_primary_key_passkey_beats_local_with_key() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xLocal".to_string(),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            ..Default::default()
        });
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: "0xPasskey".to_string(),
            key: Some(Zeroizing::new("0xpasskey_key".to_string())),
            ..Default::default()
        });
        assert_eq!(keys.primary_key().unwrap().wallet_address, "0xPasskey");
    }

    #[test]
    fn test_primary_key_passkey_without_key_still_wins() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xLocal".to_string(),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            ..Default::default()
        });
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: "0xPasskey".to_string(),
            ..Default::default()
        });
        assert_eq!(keys.primary_key().unwrap().wallet_address, "0xPasskey");
        assert!(!keys.has_wallet());
    }

    #[test]
    fn test_primary_key_inline_key_over_no_key() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xNoKey".to_string(),
            ..Default::default()
        });
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xHasKey".to_string(),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            ..Default::default()
        });
        assert_eq!(keys.primary_key().unwrap().wallet_address, "0xHasKey");
    }

    #[test]
    fn test_primary_key_empty_key_treated_as_no_key() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xEmpty".to_string(),
            key: Some(Zeroizing::new(String::new())),
            ..Default::default()
        });
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xReal".to_string(),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            ..Default::default()
        });
        assert_eq!(keys.primary_key().unwrap().wallet_address, "0xReal");
    }

    #[test]
    fn test_primary_key_first_entry_fallback_no_keys() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xFirst".to_string(),
            ..Default::default()
        });
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xSecond".to_string(),
            ..Default::default()
        });
        assert_eq!(keys.primary_key().unwrap().wallet_address, "0xFirst");
    }

    #[test]
    fn test_primary_key_empty_keys_vec() {
        let keys = Keystore::default();
        assert!(keys.primary_key().is_none());
        assert!(!keys.has_wallet());
        assert_eq!(keys.wallet_address(), "");
    }

    #[test]
    fn test_has_wallet_empty_address_with_key() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_address: String::new(),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            ..Default::default()
        });
        assert!(!keys.has_wallet());
    }

    #[test]
    fn test_key_for_network_chain_id_priority_over_passkey() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xLocal".to_string(),
            chain_id: 42431,
            key: Some(Zeroizing::new("0xlocal_key".to_string())),
            ..Default::default()
        });
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: "0xPasskey".to_string(),
            chain_id: 4217,
            key: Some(Zeroizing::new("0xpasskey_key".to_string())),
            ..Default::default()
        });
        let entry = keys.key_for_network(NetworkId::TempoModerato).unwrap();
        assert_eq!(entry.wallet_address, "0xLocal");
        let entry = keys.key_for_network(NetworkId::Tempo).unwrap();
        assert_eq!(entry.wallet_address, "0xPasskey");
    }

    #[test]
    fn test_key_for_network_direct_eoa_fallback() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: TEST_ADDRESS.to_string(),
            key_address: Some(TEST_ADDRESS.to_string()),
            key: Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string())),
            chain_id: 0,
            ..Default::default()
        });
        assert!(keys.key_for_network(NetworkId::Tempo).is_some());
        assert!(keys.key_for_network(NetworkId::TempoModerato).is_some());
    }

    #[test]
    fn test_key_for_network_no_match() {
        let keys = Keystore::default();
        assert!(keys.key_for_network(NetworkId::Tempo).is_none());
    }

    #[test]
    fn test_key_for_network_local_wrong_chain_no_fallback() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xWallet".to_string(),
            key_address: Some("0xDifferentKey".to_string()),
            key: Some(Zeroizing::new("0xkey".to_string())),
            chain_id: 4217,
            ..Default::default()
        });
        assert!(keys.key_for_network(NetworkId::Tempo).is_some());
        assert!(keys.key_for_network(NetworkId::TempoModerato).is_none());
    }

    #[test]
    fn test_key_for_network_passkey_without_key_no_fallback() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Passkey,
            wallet_address: "0xPasskey".to_string(),
            chain_id: 4217,
            ..Default::default()
        });
        assert!(keys.key_for_network(NetworkId::Tempo).is_some());
        assert!(keys.key_for_network(NetworkId::TempoModerato).is_none());
    }

    #[test]
    fn test_expiry_field_round_trip() {
        let toml_str = r#"
[[keys]]
wallet_address = "0xtest"
key = "0xaccesskey"
expiry = 1750000000
"#;
        let keys: Keystore = toml::from_str(toml_str).unwrap();
        let entry = keys.primary_key().unwrap();
        assert_eq!(entry.expiry, Some(1_750_000_000));

        let serialized = toml::to_string_pretty(&keys).unwrap();
        let parsed: Keystore = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.primary_key().unwrap().expiry, Some(1_750_000_000));
    }

    #[test]
    fn test_expiry_field_absent_defaults_to_none() {
        let toml_str = r#"
[[keys]]
wallet_address = "0xtest"
key = "0xaccesskey"
"#;
        let keys: Keystore = toml::from_str(toml_str).unwrap();
        assert_eq!(keys.primary_key().unwrap().expiry, None);
    }

    #[test]
    fn test_expiry_field_zero() {
        let toml_str = r#"
[[keys]]
wallet_address = "0xtest"
key = "0xaccesskey"
expiry = 0
"#;
        let keys: Keystore = toml::from_str(toml_str).unwrap();
        assert_eq!(keys.primary_key().unwrap().expiry, Some(0));
    }

    #[test]
    fn test_limits_round_trip() {
        let toml_str = r#"
[[keys]]
wallet_address = "0xtest"
key = "0xaccesskey"

[[keys.limits]]
currency = "0x20c000000000000000000000b9537d11c60e8b50"
limit = "100000000"

[[keys.limits]]
currency = "0x20c0000000000000000000000000000000000000"
limit = "50000000"
"#;
        let keys: Keystore = toml::from_str(toml_str).unwrap();
        let entry = keys.primary_key().unwrap();
        assert_eq!(entry.limits.len(), 2);
        assert_eq!(
            entry.limits[0].currency,
            "0x20c000000000000000000000b9537d11c60e8b50"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(entry.limits[0].limit, "100000000");
        assert_eq!(
            entry.limits[1].currency,
            "0x20c0000000000000000000000000000000000000"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(entry.limits[1].limit, "50000000");

        let serialized = toml::to_string_pretty(&keys).unwrap();
        let parsed: Keystore = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.primary_key().unwrap().limits.len(), 2);
    }

    #[test]
    fn test_limits_empty_by_default() {
        let toml_str = r#"
[[keys]]
wallet_address = "0xtest"
"#;
        let keys: Keystore = toml::from_str(toml_str).unwrap();
        assert!(keys.primary_key().unwrap().limits.is_empty());
    }

    #[test]
    fn test_delete_passkey_when_none_exists() {
        let mut keys = Keystore::default();
        keys.keys.push(KeyEntry {
            wallet_type: WalletType::Local,
            wallet_address: "0xLocal".to_string(),
            ..Default::default()
        });
        let wallet: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let err = keys.delete_passkey_wallet_address(wallet).unwrap_err();
        assert!(err.to_string().contains("No passkey wallet found"));
    }

    #[test]
    fn test_upsert_case_insensitive() {
        let mut keys = Keystore::default();
        let wallet: Address = "0x000000000000000000000000000000000000abcd"
            .parse()
            .unwrap();
        keys.keys.push(KeyEntry {
            wallet_address: "0x000000000000000000000000000000000000ABCD".to_string(),
            chain_id: 4217,
            ..Default::default()
        });
        let entry = keys.upsert_by_wallet_address_and_chain(wallet, 4217);
        entry.chain_id = 9999;
        assert_eq!(keys.keys.len(), 1);
        assert_eq!(keys.keys[0].chain_id, 9999);
    }
}
