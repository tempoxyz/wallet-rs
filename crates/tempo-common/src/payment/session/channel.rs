//! On-chain channel queries and scanning.
//!
//! Functions for querying channel state from the escrow contract
//! and scanning `ChannelOpened` events.

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{Address, Bytes, B256, U256},
    providers::Provider,
    rpc::types::{Filter, TransactionRequest},
    sol,
    sol_types::SolCall,
};

use crate::{
    config::Config,
    error::{InputError, NetworkError, TempoError},
    network::NetworkId,
};

type ChannelResult<T> = Result<T, TempoError>;

// ==================== ABI Definitions ====================

sol! {
    #[sol(rpc)]
    interface ITIP20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }
    interface IEscrow {
        function getChannel(bytes32 channelId) external view returns (
            bool finalized,
            uint64 closeRequestedAt,
            address payer,
            address payee,
            address token,
            address authorizedSigner,
            uint128 deposit,
            uint128 settled
        );
        function open(
            address payee,
            address token,
            uint128 deposit,
            bytes32 salt,
            address authorizedSigner
        ) external;
        function CLOSE_GRACE_PERIOD() external view returns (uint64 period);
        function requestClose(bytes32 channelId) external;
        function withdraw(bytes32 channelId) external;
    }
}

// ==================== Types ====================

/// On-chain channel state returned by recovery functions.
pub struct OnChainChannel {
    pub payer: Address,
    pub payee: Address,
    pub token: Address,
    pub authorized_signer: Address,
    pub deposit: u128,
    pub settled: u128,
    pub close_requested_at: u64,
}

/// Discovered on-chain channel with decoded metadata.
pub struct DiscoveredChannel {
    pub network: NetworkId,
    pub channel_id: B256,
    pub escrow_contract: Address,
    pub token: Address,
    pub deposit: u128,
    pub settled: u128,
    pub close_requested_at: u64,
}

// ==================== Constants ====================

/// Maximum block range per `eth_getLogs` query (RPC limit).
const LOG_QUERY_BLOCK_RANGE: u64 = 50_000;

/// How far back (in blocks) to scan for `ChannelOpened` events.
/// At ~2s per block this covers ~2.3 days of history.
const LOG_SCAN_DEPTH: u64 = 100_000;

/// Safety margin subtracted from `get_block_number()` to avoid
/// "block range extends beyond current head" RPC errors caused by
/// indexing lag between the node's block tip and its log index.
const LOG_HEAD_MARGIN: u64 = 10;

/// keccak256("ChannelOpened(bytes32,address,address,address,address,bytes32,uint256)")
const CHANNEL_OPENED_TOPIC: &str =
    "0xcd6e60364f8ee4c2b0d62afc07a1fb04fd267ce94693f93f8f85daaa099b5c94";

// ==================== Helpers ====================

/// Check if an RPC error indicates the block range was too large.
fn is_rpc_range_error(err: &str) -> bool {
    err.contains("query returned more than")
        || err.contains("block range")
        || err.contains("too many")
        || err.contains("exceeds max")
        || err.contains("Log response size exceeded")
}

fn should_halve_range(err: &str, chunk_start: u64, chunk_end: u64) -> bool {
    is_rpc_range_error(err) && (chunk_end - chunk_start) > 1000
}

fn split_log_query_range(chunk_start: u64, chunk_end: u64) -> Option<((u64, u64), (u64, u64))> {
    if chunk_end <= chunk_start {
        return None;
    }

    let lower_end = chunk_start + ((chunk_end - chunk_start) / 2);
    if lower_end >= chunk_end {
        return None;
    }

    Some(((chunk_start, lower_end), (lower_end + 1, chunk_end)))
}

async fn append_discovered_channels_from_logs(
    logs: &[alloy::rpc::types::Log],
    provider: &alloy::providers::RootProvider<alloy::network::Ethereum>,
    network: NetworkId,
    escrow: Address,
    results: &mut Vec<DiscoveredChannel>,
) {
    for log in logs {
        let topics = log.topics();
        if topics.len() < 4 {
            continue;
        }
        let channel_id = topics[1];

        // ChannelOpened event non-indexed data layout (ABI-encoded):
        //   [0..32]   address token       (left-padded, address at bytes 12..32)
        //   [32..64]  uint256 deposit
        //   [64..96]  bytes32 salt
        let data = log.data().data.as_ref();
        if data.len() < 96 {
            continue;
        }
        let log_token = Address::from_slice(&data[12..32]);

        let on_chain = match get_channel_on_chain(provider, escrow, channel_id).await {
            Ok(Some(ch)) => ch,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(
                    network = network.as_str(),
                    %channel_id,
                    %e,
                    "failed to query channel state, skipping"
                );
                continue;
            }
        };

        if results
            .iter()
            .any(|r: &DiscoveredChannel| r.channel_id == channel_id)
        {
            continue;
        }

        results.push(DiscoveredChannel {
            network,
            channel_id,
            escrow_contract: escrow,
            token: log_token,
            deposit: on_chain.deposit,
            settled: on_chain.settled,
            close_requested_at: on_chain.close_requested_at,
        });
    }
}

// ==================== Channel Queries ====================

/// Query the escrow contract for a specific channel's state.
///
/// Returns `Ok(None)` if `deposit == 0` or `finalized == true` (channel
/// does not exist or is already settled). Returns `Err` on RPC failures
/// so callers can distinguish "no channel" from "network error".
///
/// # Errors
///
/// Returns an error when the RPC call fails or return data cannot be decoded.
pub async fn get_channel_on_chain(
    provider: &alloy::providers::RootProvider<alloy::network::Ethereum>,
    escrow_contract: Address,
    channel_id: B256,
) -> ChannelResult<Option<OnChainChannel>> {
    let call_data = IEscrow::getChannelCall {
        channelId: channel_id,
    }
    .abi_encode();

    let tx = TransactionRequest::default()
        .to(escrow_contract)
        .input(Bytes::from(call_data).into());

    let result = provider
        .call(tx)
        .await
        .map_err(|source| NetworkError::RpcSource {
            operation: "query channel state",
            source: Box::new(source),
        })?;
    let decoded = IEscrow::getChannelCall::abi_decode_returns(&result).map_err(|source| {
        NetworkError::RpcSource {
            operation: "decode channel state",
            source: Box::new(source),
        }
    })?;

    if decoded.deposit == 0 || decoded.finalized {
        return Ok(None);
    }

    Ok(Some(OnChainChannel {
        payer: decoded.payer,
        payee: decoded.payee,
        token: decoded.token,
        authorized_signer: decoded.authorizedSigner,
        deposit: decoded.deposit,
        settled: decoded.settled,
        close_requested_at: decoded.closeRequestedAt,
    }))
}

// ==================== Event Scanning ====================

/// Scan a single network for open channels where `payer` is the sender.
///
/// This scans `ChannelOpened` events on the network's default escrow contract
/// filtered only by payer (not payee), so it finds *all* channels for this wallet.
#[allow(clippy::too_many_lines)]
pub async fn find_all_channels_for_payer(
    config: &Config,
    payer: Address,
    network: NetworkId,
) -> Vec<DiscoveredChannel> {
    let event_topic: B256 = match CHANNEL_OPENED_TOPIC.parse() {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(%e, "Invalid CHANNEL_OPENED_TOPIC constant");
            return Vec::new();
        }
    };
    let payer_topic = B256::left_padding_from(&payer.0 .0);

    let mut results = Vec::new();

    let rpc_url = config.rpc_url(network);
    let provider = alloy::providers::RootProvider::new_http(rpc_url);

    let escrow = network.escrow_contract();

    let latest = match provider.get_block_number().await {
        Ok(n) => n.saturating_sub(LOG_HEAD_MARGIN),
        Err(e) => {
            tracing::warn!(network = network.as_str(), %e, "failed to get block number");
            return results;
        }
    };
    let earliest = latest.saturating_sub(LOG_SCAN_DEPTH);

    tracing::debug!(
        network = network.as_str(),
        latest,
        earliest,
        "scanning blocks for ChannelOpened events"
    );

    let mut pending_ranges = Vec::new();
    let mut chunk_end = latest;
    while chunk_end > earliest || !pending_ranges.is_empty() {
        let ((chunk_start, query_end), from_pending_range) = match pending_ranges.pop() {
            Some(range) => (range, true),
            None => (
                (
                    chunk_end
                        .saturating_sub(LOG_QUERY_BLOCK_RANGE)
                        .max(earliest),
                    chunk_end,
                ),
                false,
            ),
        };

        let filter = Filter::new()
            .address(escrow)
            .event_signature(event_topic)
            .topic2(payer_topic)
            .from_block(BlockNumberOrTag::Number(chunk_start))
            .to_block(BlockNumberOrTag::Number(query_end));

        let logs = match provider.get_logs(&filter).await {
            Ok(logs) => logs,
            Err(e) => {
                let err_str = e.to_string();
                if should_halve_range(&err_str, chunk_start, query_end) {
                    let Some((lower, upper)) = split_log_query_range(chunk_start, query_end) else {
                        tracing::warn!(
                            network = network.as_str(),
                            chunk_start,
                            chunk_end = query_end,
                            %e,
                            "failed to split oversized log query range"
                        );
                        if !from_pending_range {
                            if chunk_start == earliest {
                                break;
                            }
                            chunk_end = chunk_start.saturating_sub(1);
                        }
                        continue;
                    };
                    tracing::debug!(
                        network = network.as_str(),
                        old_range = query_end - chunk_start,
                        lower_start = lower.0,
                        lower_end = lower.1,
                        upper_start = upper.0,
                        upper_end = upper.1,
                        "RPC range too large, halving"
                    );
                    pending_ranges.push(upper);
                    pending_ranges.push(lower);
                    if !from_pending_range {
                        if chunk_start == earliest {
                            chunk_end = earliest;
                        } else {
                            chunk_end = chunk_start.saturating_sub(1);
                        }
                    }
                    continue;
                }
                tracing::warn!(
                    network = network.as_str(),
                    chunk_start,
                    chunk_end = query_end,
                    %e,
                    "failed to query logs, skipping block range"
                );
                if from_pending_range {
                    continue;
                }
                if chunk_start == earliest {
                    break;
                }
                chunk_end = chunk_start.saturating_sub(1);
                continue;
            }
        };

        append_discovered_channels_from_logs(&logs, &provider, network, escrow, &mut results).await;

        if from_pending_range {
            continue;
        }
        if chunk_start == earliest {
            break;
        }
        chunk_end = chunk_start.saturating_sub(1);
    }

    results
}

// ==================== Channel Helpers ====================

/// Read `CLOSE_GRACE_PERIOD` from the escrow contract. Returns `None` on error.
pub async fn read_grace_period(
    provider: &alloy::providers::RootProvider<alloy::network::Ethereum>,
    escrow_contract: Address,
) -> Option<u64> {
    let call_data = IEscrow::CLOSE_GRACE_PERIODCall {}.abi_encode();
    let tx = TransactionRequest::default()
        .to(escrow_contract)
        .input(Bytes::from(call_data).into());

    let result = provider.call(tx).await.ok()?;
    let decoded: u64 = IEscrow::CLOSE_GRACE_PERIODCall::abi_decode_returns(&result).ok()?;
    Some(decoded)
}

/// Query on-chain state for a channel by its hex ID and network.
///
/// Returns `Ok(Some((token_address, deposit, settled)))` if the channel
/// exists on-chain, `Ok(None)` if confirmed not found (deposit == 0 or finalized),
/// or `Err` on RPC/config errors (callers should NOT treat errors as "missing").
///
/// # Errors
///
/// Returns an error when the channel ID is malformed, RPC access fails,
/// or on-chain channel state cannot be queried.
pub async fn query_channel_state(
    config: &Config,
    channel_id_hex: &str,
    network: NetworkId,
) -> ChannelResult<Option<(Address, u128, u128)>> {
    let channel_id: B256 =
        channel_id_hex
            .parse()
            .map_err(|_| InputError::InvalidChannelIdValue {
                value: channel_id_hex.to_string(),
            })?;
    let rpc_url = config.rpc_url(network);
    let provider = alloy::providers::RootProvider::new_http(rpc_url);
    let escrow = network.escrow_contract();

    let Some(on_chain) = get_channel_on_chain(&provider, escrow, channel_id).await? else {
        return Ok(None);
    };

    Ok(Some((on_chain.token, on_chain.deposit, on_chain.settled)))
}

/// Query the on-chain TIP20 token balance for an account.
///
/// # Errors
///
/// Returns an error when the token contract call fails.
pub async fn query_token_balance(
    provider: &impl Provider,
    token: Address,
    account: Address,
) -> ChannelResult<U256> {
    let contract = ITIP20::new(token, provider);
    let balance =
        contract
            .balanceOf(account)
            .call()
            .await
            .map_err(|source| NetworkError::RpcSource {
                operation: "query token balance",
                source: Box::new(source),
            })?;
    Ok(balance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_log_query_range_covers_both_halves_without_gap() {
        let (lower, upper) = split_log_query_range(80, 100).expect("range should split");

        assert_eq!(lower, (80, 90));
        assert_eq!(upper, (91, 100));
    }

    #[test]
    fn split_log_query_range_preserves_single_block_boundary() {
        let (lower, upper) = split_log_query_range(0, 1).expect("two block range should split");

        assert_eq!(lower, (0, 0));
        assert_eq!(upper, (1, 1));
    }

    #[test]
    fn split_log_query_range_rejects_empty_range() {
        assert_eq!(split_log_query_range(42, 42), None);
        assert_eq!(split_log_query_range(43, 42), None);
    }
}
