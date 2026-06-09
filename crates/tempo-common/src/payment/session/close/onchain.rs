//! On-chain channel close (payer-initiated `requestClose` → `withdraw`).

use alloy::{
    primitives::{Address, Bytes, TxKind, B256, U256},
    sol_types::SolCall,
};
use mpp::protocol::methods::tempo::session::ChannelDescriptor;
use tempo_primitives::transaction::Call;

use super::{
    super::{
        channel::{get_channel_on_chain, read_grace_period, IEscrow},
        store,
        store::ChannelStatus,
        tx::{build_tip1034_request_close_calls, build_tip1034_withdraw_calls, submit_tempo_tx},
        DEFAULT_GRACE_PERIOD_SECS,
    },
    CloseOutcome,
};
use crate::{
    config::Config,
    error::{InputError, PaymentError, TempoError},
    keys::{Keystore, Signer},
    network::NetworkId,
};

type ChannelResult<T> = Result<T, TempoError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseStep {
    RequestClose,
    Pending { remaining_secs: u64, ready_at: u64 },
    Withdraw,
}

fn determine_close_step(close_requested_at: u64, grace_period: u64, now: u64) -> CloseStep {
    if close_requested_at == 0 {
        return CloseStep::RequestClose;
    }

    let ready_at = close_requested_at.saturating_add(grace_period);
    if now < ready_at {
        return CloseStep::Pending {
            remaining_secs: ready_at - now,
            ready_at,
        };
    }

    CloseStep::Withdraw
}

/// Submit `requestClose()` or `withdraw()` directly on-chain as a Tempo type-0x76 transaction.
///
/// The escrow contract's payer-initiated close is a two-step process:
/// 1. `requestClose(channelId)` — starts a 15-minute grace period
/// 2. `withdraw(channelId)` — after the grace period, refunds deposit minus settled
///
/// This path works regardless of the authorized signer, since only the payer
/// wallet is required. No voucher signature is needed.
///
/// This function checks the channel's `closeRequestedAt` timestamp:
/// - If 0: submits `requestClose()` and returns `Pending`
/// - If non-zero and grace period elapsed: submits `withdraw()` and returns `Closed`
/// - If non-zero but grace period not elapsed: returns `Pending`
///
/// Close timing policy: this client enforces the on-chain grace period exactly
/// (`closeRequestedAt + gracePeriod`) and does not add an extra fixed cushion.
/// This is an intentional interoperability-first choice; strict reference-mode
/// behavior can add tighter policy checks at the CLI layer.
pub(super) async fn close_on_chain(
    config: &Config,
    wallet: &Signer,
    channel_id: B256,
    escrow_contract: Address,
    chain_id: u64,
    fee_token: Address,
    descriptor: Option<&ChannelDescriptor>,
) -> ChannelResult<CloseOutcome> {
    let network_id = NetworkId::require_chain_id(chain_id)?;
    let rpc_url = config.rpc_url(network_id);
    let provider = alloy::providers::RootProvider::new_http(rpc_url.clone());
    let tempo_provider =
        alloy::providers::RootProvider::<mpp::client::TempoNetwork>::new_http(rpc_url);

    // Check current channel state to determine which step we're on
    let on_chain = if let Some(descriptor) = descriptor {
        super::super::channel::get_precompile_channel_on_chain(
            &provider,
            escrow_contract,
            descriptor,
        )
        .await?
    } else {
        get_channel_on_chain(&provider, escrow_contract, channel_id).await?
    };
    let Some(on_chain) = on_chain else {
        return Ok(CloseOutcome::Closed {
            tx_url: None,
            amount_display: None,
        });
    };

    let from = wallet.from;

    let grace_period = read_grace_period(&provider, escrow_contract)
        .await
        .unwrap_or(DEFAULT_GRACE_PERIOD_SECS);
    let now = store::now_secs();

    match determine_close_step(on_chain.close_requested_at, grace_period, now) {
        // If closeRequestedAt is 0, we need to call requestClose() first
        CloseStep::RequestClose => {
            let calls = if let Some(descriptor) = descriptor {
                build_tip1034_request_close_calls(descriptor)?
            } else {
                let request_close_data = Bytes::from(
                    IEscrow::requestCloseCall {
                        channelId: channel_id,
                    }
                    .abi_encode(),
                );

                vec![Call {
                    to: TxKind::Call(escrow_contract),
                    value: U256::ZERO,
                    input: request_close_data,
                }]
            };

            let tx_hash =
                submit_tempo_tx(&tempo_provider, wallet, chain_id, fee_token, from, calls).await?;

            let tx_url = network_id.tx_url(&tx_hash);
            tracing::info!("requestClose TX: {}", tx_url);

            let ready_at = now + grace_period;

            // Update local channel state if present
            let _ = store::update_channel_close_state(
                &format!("{channel_id:#x}"),
                ChannelStatus::Closing,
                now,
                ready_at,
            );

            return Ok(CloseOutcome::Pending {
                remaining_secs: grace_period,
            });
        }
        // closeRequestedAt is non-zero and grace has not elapsed.
        CloseStep::Pending {
            remaining_secs,
            ready_at,
        } => {
            // Ensure pending close is persisted so `session list` can show the countdown
            // Update local channel state if present
            let _ = store::update_channel_close_state(
                &format!("{channel_id:#x}"),
                ChannelStatus::Closing,
                on_chain.close_requested_at,
                ready_at,
            );

            return Ok(CloseOutcome::Pending { remaining_secs });
        }
        CloseStep::Withdraw => {}
    }

    // Grace period elapsed — submit withdraw() to reclaim deposit
    let calls = if let Some(descriptor) = descriptor {
        build_tip1034_withdraw_calls(descriptor)?
    } else {
        let withdraw_data = Bytes::from(
            IEscrow::withdrawCall {
                channelId: channel_id,
            }
            .abi_encode(),
        );

        vec![Call {
            to: TxKind::Call(escrow_contract),
            value: U256::ZERO,
            input: withdraw_data,
        }]
    };

    let tx_hash =
        submit_tempo_tx(&tempo_provider, wallet, chain_id, fee_token, from, calls).await?;

    let tx_url = network_id.tx_url(&tx_hash);
    tracing::info!("withdraw TX: {}", tx_url);

    // Best-effort local cleanup is handled by callers, but mark state finalizable->finalized if present
    let _ = store::update_channel_close_state(
        &format!("{channel_id:#x}"),
        ChannelStatus::Finalized,
        on_chain.close_requested_at,
        now,
    );

    Ok(CloseOutcome::Closed {
        tx_url: Some(tx_url),
        amount_display: None,
    })
}

/// Close a discovered on-chain channel directly, without a server.
///
/// Uses the payer-initiated path (`requestClose` → `withdraw`) which works
/// regardless of whether the current key matches the channel's
/// `authorizedSigner`. This allows closing orphaned channels after key
/// rotation or expiry.
///
/// # Errors
///
/// Returns an error when signer resolution or on-chain close operations fail.
pub async fn close_discovered_channel(
    channel: &super::super::channel::DiscoveredChannel,
    config: &Config,
    keys: &Keystore,
) -> ChannelResult<CloseOutcome> {
    let network_id = channel.network;
    let wallet = keys.signer(network_id)?;

    close_on_chain(
        config,
        &wallet,
        channel.channel_id,
        channel.escrow_contract,
        network_id.chain_id(),
        channel.token,
        None,
    )
    .await
}

/// Close a channel by its on-chain ID.
///
/// Uses the payer-initiated path (`requestClose` → `withdraw`) which works
/// regardless of the channel's authorized signer. This allows closing
/// orphaned channels after key rotation or expiry.
///
/// # Errors
///
/// Returns an error when the channel ID is malformed, channel lookup fails,
/// signer resolution fails, or on-chain close operations fail.
pub async fn close_channel_by_id(
    config: &Config,
    channel_id_hex: &str,
    network: NetworkId,
    wallet_override: Option<&Signer>,
    keys: &Keystore,
) -> ChannelResult<CloseOutcome> {
    let channel_id: B256 =
        channel_id_hex
            .parse()
            .map_err(|_| InputError::InvalidChannelIdValue {
                value: channel_id_hex.to_string(),
            })?;

    let rpc_url = config.rpc_url(network);
    let provider = alloy::providers::RootProvider::new_http(rpc_url);

    let escrow = network.escrow_contract();

    let on_chain = get_channel_on_chain(&provider, escrow, channel_id)
        .await?
        .ok_or_else(|| PaymentError::ChannelNotFound {
            channel_id: channel_id_hex.to_string(),
            network: network.as_str().to_string(),
        })?;

    let owned_wallet;
    let wallet = if let Some(w) = wallet_override {
        w
    } else {
        owned_wallet = keys.signer(network)?;
        &owned_wallet
    };

    close_on_chain(
        config,
        wallet,
        channel_id,
        escrow,
        network.chain_id(),
        on_chain.token,
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{determine_close_step, CloseStep};

    #[test]
    fn determine_close_step_requests_close_when_not_started() {
        assert_eq!(determine_close_step(0, 900, 1_000), CloseStep::RequestClose);
    }

    #[test]
    fn determine_close_step_returns_pending_with_remaining_seconds() {
        assert_eq!(
            determine_close_step(1_000, 900, 1_200),
            CloseStep::Pending {
                remaining_secs: 700,
                ready_at: 1_900,
            }
        );
    }

    #[test]
    fn determine_close_step_returns_withdraw_when_grace_elapsed() {
        assert_eq!(determine_close_step(1_000, 900, 1_900), CloseStep::Withdraw);
        assert_eq!(determine_close_step(1_000, 900, 2_000), CloseStep::Withdraw);
    }
}
