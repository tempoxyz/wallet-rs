//! Session management: channel persistence, channel queries, close operations.
//!
//! This module provides the shared session infrastructure used by
//! tempo-wallet (session listing, closing, and management commands).
//! Request-time session orchestration (flow,
//! streaming, voucher construction) lives in `tempo-request`.
//!
//! # Module structure
//!
//! - `channel` — On-chain channel queries and event scanning
//! - `close` — Channel close operations (cooperative and on-chain)
//! - `store` — Channel persistence (`SQLite`)
//! - `tx` — Shared Tempo transaction signing and broadcast helpers

mod channel;
mod close;
mod store;
mod tx;

/// Fallback grace period (seconds) when escrow grace-period reads fail.
pub const DEFAULT_GRACE_PERIOD_SECS: u64 = 900;

// Re-export public API from `store`
pub use store::{
    delete_channel, find_reusable_channel, list_channels, load_channel, load_channels_by_origin,
    now_secs, save_channel, session_key, take_channel_store_diagnostics,
    update_channel_close_state, update_channel_cumulative_floor, ChannelRecord, ChannelStatus,
    ChannelStoreDiagnostics,
};

// Re-export public API from `channel`
pub use channel::{
    find_all_channels_for_payer, get_channel_on_chain, query_channel_state, query_token_balance,
    read_grace_period, DiscoveredChannel, OnChainChannel,
};

// Re-export public API from `close`
pub use close::{
    close_channel_by_id, close_channel_from_record, close_channel_from_record_cooperative,
    close_discovered_channel, CloseOutcome,
};

// Re-export public API from `tx`
pub use tx::{
    build_open_calls, build_tip1034_open_calls, build_tip1034_top_up_calls, build_top_up_calls,
    resolve_and_sign_tx, resolve_and_sign_tx_with_fee_payer,
    resolve_and_sign_tx_with_fee_payer_info, submit_tempo_tx, submit_tempo_tx_and_wait,
    ConfirmedTempoTx, SignedTempoTx, TempoTxReceipt,
};
