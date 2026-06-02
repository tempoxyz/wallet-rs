//! Network types and explorer configuration for Tempo blockchain networks.

use std::{fmt, str::FromStr};

use alloy::primitives::{address, Address};
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, NetworkError, TempoError};

// ==================== Constants ====================

/// Network name: Tempo mainnet.
const TEMPO: &str = "tempo";
/// Network name: Tempo Moderato testnet.
const TEMPO_MODERATO: &str = "tempo-moderato";

/// EVM chain ID: Tempo mainnet.
const TEMPO_CHAIN_ID: u64 = 4217;
/// EVM chain ID: Tempo Moderato testnet.
const TEMPO_MODERATO_CHAIN_ID: u64 = 42431;

/// pathUSD token address (testnet).
const PATH_USD_TOKEN: Address = address!("20c0000000000000000000000000000000000000");
/// USDC.e token address (mainnet).
pub const USDCE_TOKEN: Address = address!("20c000000000000000000000b9537d11c60e8b50");
/// Escrow contract address (mainnet).
const TEMPO_ESCROW: Address = address!("33b901018174ddabe4841042ab76ba85d4e24f25");
/// Escrow contract address (moderato testnet).
pub const TEMPO_MODERATO_ESCROW: Address = address!("e1c4d3dce17bc111181ddf716f75bae49e61a336");

/// Token configuration for Tempo mainnet (USDC.e).
const TEMPO_TOKEN: TokenConfig = TokenConfig {
    symbol: "USDC.e",
    decimals: 6,
    address: USDCE_TOKEN,
};

/// Token configuration for Tempo Moderato testnet (pathUSD).
const TEMPO_MODERATO_TOKEN: TokenConfig = TokenConfig {
    symbol: "pathUSD",
    decimals: 6,
    address: PATH_USD_TOKEN,
};

// ==================== Network Types ====================

/// Token configuration for a network.
#[derive(Debug, Clone, Copy)]
pub struct TokenConfig {
    /// Token symbol (e.g., "USDC.e", "pathUSD")
    pub symbol: &'static str,
    /// Number of decimal places
    pub decimals: u8,
    /// Token address - contract address for EVM chains (ERC20)
    pub address: Address,
}

/// Static network identifier with compile-time metadata.
///
/// A lightweight `Copy` enum for network identity. All static metadata
/// (chain ID, escrow contract, tokens, default RPC, explorer) lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NetworkId {
    #[default]
    Tempo,
    TempoModerato,
}

impl NetworkId {
    /// Resolve an optional network name to a `NetworkId`, defaulting to Tempo mainnet.
    ///
    /// # Errors
    ///
    /// Returns an error when a provided network string does not map to a
    /// supported `NetworkId`.
    pub fn resolve(network: Option<&str>) -> Result<Self, TempoError> {
        network.map_or_else(
            || Ok(Self::Tempo),
            |s| s.parse::<Self>().map_err(TempoError::from),
        )
    }

    /// Get the string identifier for this network.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Tempo => TEMPO,
            Self::TempoModerato => TEMPO_MODERATO,
        }
    }

    /// Get the browser-auth network slug.
    ///
    /// This is intentionally user-facing (`mainnet` / `testnet`) instead of
    /// the internal network id (`tempo` / `tempo-moderato`) because the wallet
    /// web app uses this value to select the login environment.
    #[must_use]
    pub const fn auth_network_slug(&self) -> &'static str {
        match self {
            Self::Tempo => "mainnet",
            Self::TempoModerato => "testnet",
        }
    }

    /// Get the chain ID for this network.
    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        match self {
            Self::Tempo => TEMPO_CHAIN_ID,
            Self::TempoModerato => TEMPO_MODERATO_CHAIN_ID,
        }
    }

    /// Look up a network by its EVM chain ID.
    #[must_use]
    pub const fn from_chain_id(chain_id: u64) -> Option<Self> {
        match chain_id {
            TEMPO_CHAIN_ID => Some(Self::Tempo),
            TEMPO_MODERATO_CHAIN_ID => Some(Self::TempoModerato),
            _ => None,
        }
    }

    /// Look up a network by chain ID, returning an error for unsupported chains.
    ///
    /// # Errors
    ///
    /// Returns an error when `chain_id` is not one of the built-in Tempo networks.
    pub fn require_chain_id(chain_id: u64) -> Result<Self, TempoError> {
        Self::from_chain_id(chain_id)
            .ok_or_else(|| ConfigError::UnsupportedChainId(chain_id).into())
    }

    /// Get the default RPC URL for this network.
    #[must_use]
    pub const fn default_rpc_url(&self) -> &'static str {
        match self {
            Self::Tempo => "https://rpc.mainnet.tempo.xyz",
            Self::TempoModerato => "https://rpc.moderato.tempo.xyz",
        }
    }

    /// Get the auth server URL for browser-based wallet authentication.
    #[must_use]
    pub const fn auth_url(&self) -> &'static str {
        match self {
            Self::Tempo => "https://wallet.tempo.xyz/cli-auth",
            Self::TempoModerato => "https://wallet.tempo.xyz/cli-auth",
        }
    }

    /// Get the block explorer base URL for this network.
    const fn explorer_base_url(self) -> &'static str {
        match self {
            Self::Tempo => "https://explore.tempo.xyz",
            Self::TempoModerato => "https://explore.moderato.tempo.xyz",
        }
    }

    /// Build a transaction URL on the block explorer.
    #[must_use]
    pub fn tx_url(&self, hash: &str) -> String {
        format!("{}/receipt/{}", self.explorer_base_url(), hash)
    }

    /// Build an address URL on the block explorer.
    #[must_use]
    pub fn address_url(&self, addr: &str) -> String {
        format!("{}/address/{}", self.explorer_base_url(), addr)
    }

    /// Get the default escrow contract address for this network.
    ///
    /// These match the addresses in `mpp::client::channel_ops::default_escrow_contract`.
    #[must_use]
    pub const fn escrow_contract(&self) -> Address {
        match self {
            Self::Tempo => TEMPO_ESCROW,
            Self::TempoModerato => TEMPO_MODERATO_ESCROW,
        }
    }

    /// Get the payment token for this network.
    #[must_use]
    pub const fn token(&self) -> &'static TokenConfig {
        match self {
            Self::Tempo => &TEMPO_TOKEN,
            Self::TempoModerato => &TEMPO_MODERATO_TOKEN,
        }
    }
}

impl FromStr for NetworkId {
    type Err = NetworkError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_network_name(s).ok_or_else(|| NetworkError::UnknownNetwork(s.to_string()))
    }
}

fn parse_network_name(value: &str) -> Option<NetworkId> {
    let normalized = value.trim();
    if normalized.eq_ignore_ascii_case(TEMPO) || normalized.eq_ignore_ascii_case("mainnet") {
        return Some(NetworkId::Tempo);
    }
    if normalized.eq_ignore_ascii_case(TEMPO_MODERATO)
        || normalized.eq_ignore_ascii_case("testnet")
        || normalized.eq_ignore_ascii_case("moderato")
    {
        return Some(NetworkId::TempoModerato);
    }
    None
}

impl fmt::Display for NetworkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Serialize for NetworkId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NetworkId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tempo_urls() {
        assert_eq!(
            NetworkId::Tempo.tx_url("0xabc123"),
            "https://explore.tempo.xyz/receipt/0xabc123"
        );
        assert_eq!(
            NetworkId::Tempo.address_url("0x742d35Cc"),
            "https://explore.tempo.xyz/address/0x742d35Cc"
        );
    }

    #[test]
    fn test_network_id_from_str() {
        assert_eq!(
            "tempo".parse::<NetworkId>().expect("Failed to parse tempo"),
            NetworkId::Tempo
        );
        assert_eq!(
            "tempo-moderato"
                .parse::<NetworkId>()
                .expect("Failed to parse tempo-moderato"),
            NetworkId::TempoModerato
        );
        assert!("tempo-localnet".parse::<NetworkId>().is_err());
        assert!("unknown-network".parse::<NetworkId>().is_err());
    }

    #[test]
    fn test_network_id_to_str() {
        assert_eq!(NetworkId::Tempo.as_str(), "tempo");
        assert_eq!(NetworkId::TempoModerato.as_str(), "tempo-moderato");
        assert_eq!(NetworkId::Tempo.to_string(), "tempo");
    }

    #[test]
    fn test_auth_network_slug() {
        assert_eq!(NetworkId::Tempo.auth_network_slug(), "mainnet");
        assert_eq!(NetworkId::TempoModerato.auth_network_slug(), "testnet");
    }

    #[test]
    fn test_network_id_info() {
        let tempo = NetworkId::Tempo;
        assert_eq!(tempo.chain_id(), 4217);

        let moderato = NetworkId::TempoModerato;
        assert_eq!(moderato.chain_id(), 42431);
    }

    #[test]
    fn test_network_id_roundtrip() {
        for network_str in &["tempo", "tempo-moderato"] {
            let id: NetworkId = network_str.parse().expect("should parse");
            assert_eq!(id.as_str(), *network_str);
            assert_eq!(id.to_string(), *network_str);
        }
    }

    #[test]
    fn test_token_mainnet() {
        let token = NetworkId::Tempo.token();
        assert_eq!(token.symbol, "USDC.e");
        assert_eq!(token.address, USDCE_TOKEN);
    }

    #[test]
    fn test_token_testnet() {
        let token = NetworkId::TempoModerato.token();
        assert_eq!(token.symbol, "pathUSD");
        assert_eq!(token.address, PATH_USD_TOKEN);
    }

    #[test]
    fn test_from_chain_id() {
        assert_eq!(NetworkId::from_chain_id(4217), Some(NetworkId::Tempo));
        assert_eq!(
            NetworkId::from_chain_id(42431),
            Some(NetworkId::TempoModerato)
        );
        assert_eq!(NetworkId::from_chain_id(1337), None);
        assert_eq!(NetworkId::from_chain_id(99999), None);
    }

    #[test]
    fn test_network_name_case_insensitive() {
        assert!("Tempo".parse::<NetworkId>().is_ok());
        assert!("TEMPO".parse::<NetworkId>().is_ok());
        assert!("TEMPO-MODERATO".parse::<NetworkId>().is_ok());
    }

    #[test]
    fn test_network_name_trimmed() {
        assert_eq!(
            "  tempo-moderato  ".parse::<NetworkId>().unwrap(),
            NetworkId::TempoModerato
        );
    }

    #[test]
    fn test_resolve_defaults_to_tempo() {
        assert_eq!(NetworkId::resolve(None).unwrap(), NetworkId::Tempo);
    }

    #[test]
    fn test_resolve_known_networks() {
        assert_eq!(NetworkId::resolve(Some("tempo")).unwrap(), NetworkId::Tempo);
        assert_eq!(
            NetworkId::resolve(Some("tempo-moderato")).unwrap(),
            NetworkId::TempoModerato
        );
    }

    #[test]
    fn test_resolve_unknown_network() {
        assert!(NetworkId::resolve(Some("unknown")).is_err());
    }

    #[test]
    fn test_resolve_case_insensitive() {
        assert_eq!(NetworkId::resolve(Some("TEMPO")).unwrap(), NetworkId::Tempo);
    }

    #[test]
    fn test_require_chain_id_known() {
        assert_eq!(NetworkId::require_chain_id(4217).unwrap(), NetworkId::Tempo);
        assert_eq!(
            NetworkId::require_chain_id(42431).unwrap(),
            NetworkId::TempoModerato
        );
    }

    #[test]
    fn test_require_chain_id_unknown() {
        assert!(NetworkId::require_chain_id(9999).is_err());
    }

    #[test]
    fn test_escrow_contract_addresses() {
        let tempo = NetworkId::Tempo.escrow_contract();
        let moderato = NetworkId::TempoModerato.escrow_contract();
        assert_eq!(tempo, TEMPO_ESCROW);
        assert_eq!(moderato, TEMPO_MODERATO_ESCROW);
        assert_ne!(tempo, moderato);
    }

    #[test]
    fn test_default_rpc_url() {
        assert!(NetworkId::Tempo.default_rpc_url().starts_with("https://"));
        assert!(NetworkId::TempoModerato
            .default_rpc_url()
            .starts_with("https://"));
    }

    #[test]
    fn test_serde_roundtrip() {
        let json = serde_json::to_string(&NetworkId::Tempo).unwrap();
        assert_eq!(json, "\"tempo\"");
        let back: NetworkId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, NetworkId::Tempo);

        let json = serde_json::to_string(&NetworkId::TempoModerato).unwrap();
        assert_eq!(json, "\"tempo-moderato\"");
        let back: NetworkId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, NetworkId::TempoModerato);
    }

    #[test]
    fn test_auth_url() {
        assert!(NetworkId::Tempo.auth_url().contains("wallet.tempo.xyz"));
        assert_eq!(
            NetworkId::TempoModerato.auth_url(),
            "https://wallet.tempo.xyz/cli-auth"
        );
    }

    #[test]
    fn test_moderato_explorer_urls() {
        assert!(NetworkId::TempoModerato
            .tx_url("0xdef456")
            .contains("explore.moderato.tempo.xyz"));
        assert!(NetworkId::TempoModerato
            .address_url("0x999aaa")
            .contains("explore.moderato.tempo.xyz"));
    }
}
