//! Domain model and helpers for persisted channel records.

use alloy::primitives::{Address, B256};
use mpp::protocol::methods::tempo::session::{ChannelDescriptor, SESSION_PROTOCOL_LEGACY};
use serde::{Deserialize, Serialize};

use crate::network::NetworkId;

/// Error returned when decoding a channel status from persisted storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InvalidChannelStatusError {
    value: String,
}

impl InvalidChannelStatusError {
    fn new(value: &str) -> Self {
        Self {
            value: value.to_string(),
        }
    }
}

impl std::fmt::Display for InvalidChannelStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid channel status '{}'", self.value)
    }
}

impl std::error::Error for InvalidChannelStatusError {}

/// Channel lifecycle state.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChannelStatus {
    #[default]
    Active,
    Closing,
    Finalizable,
    Finalized,
    Orphaned,
}

impl ChannelStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Closing => "closing",
            Self::Finalizable => "finalizable",
            Self::Finalized => "finalized",
            Self::Orphaned => "orphaned",
        }
    }

    pub(super) fn try_from_db_str(value: &str) -> Result<Self, InvalidChannelStatusError> {
        match value {
            "active" => Ok(Self::Active),
            "closing" => Ok(Self::Closing),
            "finalizable" => Ok(Self::Finalizable),
            "finalized" => Ok(Self::Finalized),
            "orphaned" => Ok(Self::Orphaned),
            _ => Err(InvalidChannelStatusError::new(value)),
        }
    }
}

/// A persisted payment channel record.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChannelRecord {
    #[serde(default = "default_version")]
    pub version: u32,
    pub origin: String,
    #[serde(default)]
    pub request_url: String,
    pub chain_id: u64,
    pub escrow_contract: Address,
    pub token: String,
    pub payee: String,
    pub payer: String,
    pub authorized_signer: Address,
    pub salt: String,
    pub channel_id: B256,
    #[serde(default = "default_session_protocol")]
    pub session_protocol: String,
    #[serde(default)]
    pub descriptor_json: Option<String>,
    pub deposit: u128,
    pub cumulative_amount: u128,
    /// Server-confirmed accepted cumulative amount. Reflects the highest voucher
    /// the server has accepted. Defaults to `cumulative_amount` when unknown.
    #[serde(default)]
    pub accepted_cumulative: u128,
    /// Server-reported actual spend. Read from the `spent` field of the
    /// `Payment-Receipt` header. Used for cooperative close to avoid
    /// overcharging. Defaults to 0 when the server has not reported spend.
    #[serde(default)]
    pub server_spent: u128,
    pub challenge_echo: String,
    /// Explicit lifecycle state.
    #[serde(default = "default_state")]
    pub state: ChannelStatus,
    /// UNIX time when close was requested (0 if not requested)
    #[serde(default)]
    pub close_requested_at: u64,
    /// UNIX time when channel is ready to finalize (0 if not applicable)
    #[serde(default)]
    pub grace_ready_at: u64,
    pub created_at: u64,
    pub last_used_at: u64,
}

const fn default_version() -> u32 {
    1
}

const fn default_state() -> ChannelStatus {
    ChannelStatus::Active
}

fn default_session_protocol() -> String {
    SESSION_PROTOCOL_LEGACY.to_string()
}

#[must_use]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ChannelRecord {
    /// Parse the cumulative amount.
    #[must_use]
    pub const fn cumulative_amount_u128(&self) -> u128 {
        self.cumulative_amount
    }

    /// Parse the deposit amount.
    #[must_use]
    pub const fn deposit_u128(&self) -> u128 {
        self.deposit
    }

    /// Parse the channel ID.
    #[must_use]
    pub const fn channel_id_b256(&self) -> B256 {
        self.channel_id
    }

    /// Canonical lowercase hex representation of channel ID.
    #[must_use]
    pub fn channel_id_hex(&self) -> String {
        format!("{channel_id:#x}", channel_id = self.channel_id)
    }

    #[must_use]
    pub fn session_protocol_or_legacy(&self) -> &str {
        match self.session_protocol.as_str() {
            "" | "legacy" => SESSION_PROTOCOL_LEGACY,
            protocol => protocol,
        }
    }

    pub fn descriptor(&self) -> Option<ChannelDescriptor> {
        self.descriptor_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
    }

    /// The server-confirmed accepted amount.
    /// Falls back to `cumulative_amount` when the accepted amount is unknown (zero).
    #[must_use]
    pub const fn accepted_cumulative_u128(&self) -> u128 {
        if self.accepted_cumulative > 0 {
            self.accepted_cumulative
        } else {
            self.cumulative_amount
        }
    }

    /// The amount to use for cooperative close. Prefers the server-reported
    /// actual spend over the accepted voucher ceiling to avoid overcharging.
    #[must_use]
    pub const fn close_amount(&self) -> u128 {
        if self.server_spent > 0 {
            self.server_spent
        } else {
            self.accepted_cumulative_u128()
        }
    }

    /// Update the cumulative amount (monotonic: never decreases).
    pub fn set_cumulative_amount(&mut self, amount: u128) {
        self.cumulative_amount = amount.max(self.cumulative_amount);
    }

    /// Update the accepted cumulative amount (monotonic: never decreases).
    pub fn set_accepted_cumulative(&mut self, amount: u128) {
        self.accepted_cumulative = amount.max(self.accepted_cumulative);
    }

    /// Update the server-reported spend. Always takes the latest value from
    /// the server (not monotonic — spend can be reconciled to a lower amount
    /// when a reservation is committed at actual cost).
    pub fn set_server_spent(&mut self, amount: u128) {
        self.server_spent = amount;
    }

    /// Update `last_used_at` timestamp.
    pub fn touch(&mut self) {
        self.last_used_at = now_secs();
    }

    /// Derive the network from `chain_id`.
    #[must_use]
    pub fn network_id(&self) -> NetworkId {
        NetworkId::from_chain_id(self.chain_id).unwrap_or_default()
    }

    /// Validate and canonicalize persisted string identity fields.
    ///
    /// Returns `false` when `token` or `payee` is not a valid address.
    pub fn normalize_persisted_identity(&mut self) -> bool {
        let Ok(token) = self.token.parse::<Address>() else {
            return false;
        };
        let Ok(payee) = self.payee.parse::<Address>() else {
            return false;
        };

        self.token = format!("{token:#x}");
        self.payee = format!("{payee:#x}");
        true
    }

    /// Compute the display status and optional remaining seconds from channel state.
    ///
    /// Returns `(status, remaining_secs)`:
    /// - Active channels: `(ChannelStatus::Active, None)`
    /// - Closing with time remaining: `(ChannelStatus::Closing, Some(secs))`
    /// - Closing with grace elapsed: `(ChannelStatus::Finalizable, Some(0))`
    #[must_use]
    pub const fn status_at(&self, now: u64) -> (ChannelStatus, Option<u64>) {
        match self.state {
            ChannelStatus::Closing => {
                let rem = self.grace_ready_at.saturating_sub(now);
                if rem == 0 && self.grace_ready_at > 0 {
                    (ChannelStatus::Finalizable, Some(0))
                } else {
                    (ChannelStatus::Closing, Some(rem))
                }
            }
            ChannelStatus::Finalizable => (ChannelStatus::Finalizable, Some(0)),
            ChannelStatus::Finalized => (ChannelStatus::Finalized, None),
            ChannelStatus::Orphaned => (ChannelStatus::Orphaned, None),
            ChannelStatus::Active => (ChannelStatus::Active, None),
        }
    }
}

/// Compute an origin lock key from the origin URL (extract `scheme://host[:port]`).
///
/// Non-alphanumeric chars (except `-` and `.`) are replaced with `_`.
#[must_use]
pub fn session_key(origin: &str) -> String {
    let normalized = url::Url::parse(origin)
        .map_or_else(|_| origin.to_string(), |u| u.origin().ascii_serialization());

    normalized
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_record(origin: &str, salt: &str) -> ChannelRecord {
        let now = now_secs();
        ChannelRecord {
            version: 1,
            origin: origin.into(),
            request_url: format!("{origin}/api/v1"),
            chain_id: 4217,
            escrow_contract: Address::ZERO,
            token: "0x00".into(),
            payee: "0x00".into(),
            payer: "0x00".into(),
            authorized_signer: Address::ZERO,
            salt: salt.into(),
            channel_id: B256::ZERO,
            session_protocol: SESSION_PROTOCOL_LEGACY.to_string(),
            descriptor_json: None,
            deposit: 1_000_000,
            cumulative_amount: 0,
            accepted_cumulative: 0,
            server_spent: 0,
            challenge_echo: "echo".into(),
            state: ChannelStatus::Active,
            close_requested_at: 0,
            grace_ready_at: 0,
            created_at: now,
            last_used_at: now,
        }
    }

    #[test]
    fn test_touch_updates_last_used() {
        let mut record = test_record("https://example.com", "salt");
        record.last_used_at = 1000;
        record.touch();
        assert!(record.last_used_at > 1000);
    }

    #[test]
    fn test_session_protocol_or_legacy_returns_v1() {
        let mut record = test_record("https://example.com", "salt");
        assert_eq!(record.session_protocol_or_legacy(), SESSION_PROTOCOL_LEGACY);

        record.session_protocol = String::new();
        assert_eq!(record.session_protocol_or_legacy(), SESSION_PROTOCOL_LEGACY);

        record.session_protocol = "legacy".to_string();
        assert_eq!(record.session_protocol_or_legacy(), SESSION_PROTOCOL_LEGACY);
    }

    #[test]
    fn test_status_at_active() {
        let record = test_record("https://example.com", "salt");
        let (status, rem) = record.status_at(1000);
        assert_eq!(status, ChannelStatus::Active);
        assert!(rem.is_none());
    }

    #[test]
    fn test_status_at_closing_with_remaining() {
        let mut record = test_record("https://example.com", "salt");
        record.state = ChannelStatus::Closing;
        record.grace_ready_at = 2000;
        let (status, rem) = record.status_at(1500);
        assert_eq!(status, ChannelStatus::Closing);
        assert_eq!(rem, Some(500));
    }

    #[test]
    fn test_status_at_closing_grace_elapsed() {
        let mut record = test_record("https://example.com", "salt");
        record.state = ChannelStatus::Closing;
        record.grace_ready_at = 1000;
        let (status, rem) = record.status_at(2000);
        assert_eq!(status, ChannelStatus::Finalizable);
        assert_eq!(rem, Some(0));
    }

    #[test]
    fn test_status_at_finalizable() {
        let mut record = test_record("https://example.com", "salt");
        record.state = ChannelStatus::Finalizable;
        let (status, rem) = record.status_at(5000);
        assert_eq!(status, ChannelStatus::Finalizable);
        assert_eq!(rem, Some(0));
    }

    #[test]
    fn test_status_at_finalized() {
        let mut record = test_record("https://example.com", "salt");
        record.state = ChannelStatus::Finalized;
        let (status, rem) = record.status_at(1000);
        assert_eq!(status, ChannelStatus::Finalized);
        assert!(rem.is_none());
    }

    #[test]
    fn test_status_at_orphaned() {
        let mut record = test_record("https://example.com", "salt");
        record.state = ChannelStatus::Orphaned;
        let (status, rem) = record.status_at(1000);
        assert_eq!(status, ChannelStatus::Orphaned);
        assert!(rem.is_none());
    }

    #[test]
    fn test_network_id_tempo() {
        let record = test_record("https://example.com", "salt");
        assert_eq!(record.chain_id, 4217);
        assert_eq!(record.network_id(), NetworkId::Tempo);
    }

    #[test]
    fn test_network_id_moderato() {
        let mut record = test_record("https://example.com", "salt");
        record.chain_id = 42431;
        assert_eq!(record.network_id(), NetworkId::TempoModerato);
    }

    #[test]
    fn test_cumulative_amount_u128_valid() {
        let mut record = test_record("https://example.com", "salt");
        record.cumulative_amount = 1000;
        assert_eq!(record.cumulative_amount_u128(), 1000u128);
    }

    #[test]
    fn test_set_cumulative_amount_monotonic() {
        let mut record = test_record("https://example.com", "salt");
        record.cumulative_amount = 50;
        record.set_cumulative_amount(10);
        assert_eq!(record.cumulative_amount, 50);
        record.set_cumulative_amount(100);
        assert_eq!(record.cumulative_amount, 100);
    }

    #[test]
    fn test_close_amount_prefers_server_spent() {
        let mut record = test_record("https://example.com", "salt");
        record.cumulative_amount = 100;
        record.accepted_cumulative = 80;
        record.server_spent = 55;
        assert_eq!(record.close_amount(), 55);
    }

    #[test]
    fn test_close_amount_falls_back_when_server_spent_missing() {
        let mut record = test_record("https://example.com", "salt");
        record.cumulative_amount = 100;
        record.accepted_cumulative = 80;
        record.server_spent = 0;
        assert_eq!(record.close_amount(), 80);

        record.accepted_cumulative = 0;
        assert_eq!(record.close_amount(), 100);
    }

    #[test]
    fn test_deposit_u128_valid() {
        let mut record = test_record("https://example.com", "salt");
        record.deposit = 5_000_000;
        assert_eq!(record.deposit_u128(), 5_000_000_u128);
    }

    #[test]
    fn test_channel_id_hex() {
        let mut record = test_record("https://example.com", "salt");
        record.channel_id = B256::from(alloy::primitives::U256::from(1));
        assert_eq!(
            record.channel_id_hex(),
            "0x0000000000000000000000000000000000000000000000000000000000000001"
        );
    }

    #[test]
    fn test_channel_id_b256_valid() {
        let mut record = test_record("https://example.com", "salt");
        record.channel_id = B256::from(alloy::primitives::U256::from(1));
        let b = record.channel_id_b256();
        assert_eq!(b, B256::from(alloy::primitives::U256::from(1)));
    }

    #[test]
    fn test_channel_status_round_trip() {
        let variants = [
            ChannelStatus::Active,
            ChannelStatus::Closing,
            ChannelStatus::Finalizable,
            ChannelStatus::Finalized,
            ChannelStatus::Orphaned,
        ];
        for variant in variants {
            let s = variant.as_str();
            let parsed = ChannelStatus::try_from_db_str(s).unwrap();
            assert_eq!(parsed, variant, "round-trip failed for {s}");
        }
    }

    #[test]
    fn test_channel_status_unknown_is_error() {
        let err = ChannelStatus::try_from_db_str("garbage").unwrap_err();
        assert_eq!(err.to_string(), "invalid channel status 'garbage'");

        let err = ChannelStatus::try_from_db_str("").unwrap_err();
        assert_eq!(err.to_string(), "invalid channel status ''");
    }

    #[test]
    fn test_normalize_persisted_identity_canonicalizes_addresses() {
        let mut record = test_record("https://example.com", "salt");
        record.token = "0x20C000000000000000000000B9537D11C60E8B50".into();
        record.payee = "0x111111111111111111111111111111111111AbCd".into();

        assert!(record.normalize_persisted_identity());
        assert_eq!(record.token, "0x20c000000000000000000000b9537d11c60e8b50");
        assert_eq!(record.payee, "0x111111111111111111111111111111111111abcd");
    }

    #[test]
    fn test_normalize_persisted_identity_rejects_invalid_addresses() {
        let mut record = test_record("https://example.com", "salt");
        record.token = "bad-token".into();
        assert!(!record.normalize_persisted_identity());

        let mut record = test_record("https://example.com", "salt");
        record.payee = "bad-payee".into();
        assert!(!record.normalize_persisted_identity());
    }

    #[test]
    fn test_session_key_basic() {
        assert_eq!(
            session_key("https://api.example.com/v1/chat"),
            "https___api.example.com"
        );
    }

    #[test]
    fn test_session_key_with_port() {
        assert_eq!(
            session_key("http://localhost:8080/foo"),
            "http___localhost_8080"
        );
    }

    #[test]
    fn test_session_key_no_path() {
        assert_eq!(session_key("https://example.com"), "https___example.com");
    }

    #[test]
    fn test_session_key_different_paths_same_origin() {
        assert_eq!(
            session_key("https://example.com/v1/chat"),
            session_key("https://example.com/v2/other")
        );
        assert_eq!(
            session_key("https://example.com/a?foo=bar"),
            session_key("https://example.com/b#frag")
        );
    }

    #[test]
    fn test_session_key_trailing_slash() {
        assert_eq!(
            session_key("https://example.com/"),
            session_key("https://example.com")
        );
    }

    #[test]
    fn test_session_key_query_params_stripped() {
        assert_eq!(
            session_key("https://example.com/path?foo=bar"),
            session_key("https://example.com/other")
        );
    }
}
