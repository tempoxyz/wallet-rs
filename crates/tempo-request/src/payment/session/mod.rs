//! Session payment request handling.
//!
//! Contains the request-time session logic: flow orchestration,
//! streaming, transaction building, and voucher construction.
//! Session persistence, channel queries, and close operations
//! remain in `tempo_common::payment::session`.

mod error_map;
mod flow;
mod open;
mod persist;
mod receipt;
mod streaming;
mod voucher;

pub(super) use flow::handle_session_request;

use alloy::primitives::{Address, B256};
use mpp::protocol::methods::tempo::session::ChannelDescriptor;

use crate::http::HttpClient;
use tempo_common::{keys::Signer, network::NetworkId};

/// State for an active session channel.
pub(super) struct ChannelState {
    pub(super) channel_id: B256,
    pub(super) escrow_contract: Address,
    pub(super) chain_id: u64,
    pub(super) deposit: u128,
    pub(super) cumulative_amount: u128,
    pub(super) accepted_cumulative: u128,
    /// Optional hard cap on cumulative session spend.
    pub(super) max_cumulative_spend: Option<u128>,
    /// Server-reported actual spend from `Payment-Receipt`. Used at close to
    /// avoid overcharging when the server reconciles below the voucher ceiling.
    pub(super) server_spent: u128,
    pub(super) session_protocol: String,
    pub(super) descriptor: Option<ChannelDescriptor>,
}

impl ChannelState {
    pub(super) fn is_tip1034(&self) -> bool {
        self.session_protocol == mpp::protocol::methods::tempo::session::SESSION_PROTOCOL_TIP1034
    }
}

/// Shared context for session operations (streaming, closing).
pub(super) struct ChannelContext<'a> {
    pub(super) signer: &'a Signer,
    pub(super) payer: Address,
    pub(super) echo: &'a mpp::ChallengeEcho,
    pub(super) did: &'a str,
    pub(super) http: &'a HttpClient,
    pub(super) url: &'a str,
    pub(super) rpc_url: &'a str,
    pub(super) network_id: NetworkId,
    pub(super) origin: &'a str,
    pub(super) top_up_deposit: u128,
    pub(super) clamped_deposit: Option<u128>,
    pub(super) fee_payer: bool,
    pub(super) salt: String,
    pub(super) payee: Address,
    pub(super) token: Address,
    /// Shared reqwest client for connection pooling across session requests.
    pub(super) reqwest_client: &'a reqwest::Client,
}

/// Extract the origin (<scheme://host>\[:port\]) from a URL.
fn extract_origin(url: &str) -> String {
    url::Url::parse(url).map_or_else(|_| url.to_string(), |u| u.origin().ascii_serialization())
}

fn new_idempotency_key() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        bytes.copy_from_slice(&nanos.to_be_bytes());
    }

    // RFC 4122 version 4 UUID layout.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}
