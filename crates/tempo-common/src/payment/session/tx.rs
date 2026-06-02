//! Shared Tempo transaction signing and broadcast helpers.
//!
//! Low-level Tempo type-0x76 transaction construction and receipt polling.
//! All transactions use expiring nonces (nonceKey=MAX, nonce=0) so no
//! on-chain nonce fetch is needed.

use alloy::{
    primitives::{Address, Bytes, TxKind, B256, U256},
    providers::Provider,
    sol,
    sol_types::SolCall,
};
use tempo_primitives::transaction::Call;

use mpp::{
    client::tempo::{charge::tx_builder, signing},
    protocol::methods::tempo::transaction::TempoTransactionRequest,
};

use crate::{
    error::{KeyError, NetworkError, TempoError},
    keys::Signer,
    payment::classify::classify_tempo_rpc_error,
};

type ChannelResult<T> = Result<T, TempoError>;

// ==================== ABI Definitions ====================

sol! {
    interface ITIP20 {
        function approve(address spender, uint256 amount) external returns (bool);
    }
    interface IEscrow {
        function open(
            address payee,
            address token,
            uint128 deposit,
            bytes32 salt,
            address authorizedSigner
        ) external;
        function topUp(bytes32 channelId, uint256 additionalDeposit) external;
    }
}

/// Static max fee per gas (41 gwei) — Tempo uses a fixed 20 gwei base fee.
const MAX_FEE_PER_GAS: u128 = mpp::client::tempo::MAX_FEE_PER_GAS;

/// Static max priority fee per gas (1 gwei).
const MAX_PRIORITY_FEE_PER_GAS: u128 = mpp::client::tempo::MAX_PRIORITY_FEE_PER_GAS;

/// Expiring nonce key (`U256::MAX`).
const EXPIRING_NONCE_KEY: U256 = U256::MAX;

/// Validity window (in seconds) for expiring nonce transactions.
const VALID_BEFORE_SECS: u64 = 25;

/// Compute the expiring nonce validity window.
fn expiring_valid_before() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + VALID_BEFORE_SECS
}

fn classify_tx_error(err: &impl std::fmt::Display) -> Option<TempoError> {
    classify_tempo_rpc_error(err.to_string())
}

/// Estimate gas, build and sign a Tempo type-0x76 transaction.
///
/// Uses expiring nonces (nonceKey=MAX, nonce=0) and static gas fees
/// (Tempo has a fixed 20 gwei base fee), so only a single RPC call
/// (`eth_estimateGas`) is needed.
///
/// # Errors
///
/// Returns an error when gas estimation, transaction signing, or encoding fails.
pub async fn resolve_and_sign_tx(
    provider: &alloy::providers::RootProvider<mpp::client::TempoNetwork>,
    wallet: &Signer,
    chain_id: u64,
    fee_token: Address,
    from: Address,
    calls: Vec<tempo_primitives::transaction::Call>,
) -> ChannelResult<Vec<u8>> {
    resolve_and_sign_tx_with_fee_payer(provider, wallet, chain_id, fee_token, from, calls, false)
        .await
}

/// Estimate gas, build and sign a Tempo type-0x76 transaction, optionally in fee-payer mode.
///
/// When `fee_payer` is `true`, the transaction is constructed without a fee token and with
/// a placeholder fee-payer signature so a sponsor can co-sign server-side.
pub async fn resolve_and_sign_tx_with_fee_payer(
    provider: &alloy::providers::RootProvider<mpp::client::TempoNetwork>,
    wallet: &Signer,
    chain_id: u64,
    fee_token: Address,
    from: Address,
    calls: Vec<tempo_primitives::transaction::Call>,
    fee_payer: bool,
) -> ChannelResult<Vec<u8>> {
    let nonce = 0u64;
    let valid_before = Some(expiring_valid_before());
    // The final transaction uses Address::ZERO when fee_payer is true (the sponsor
    // co-signs server-side), but gas estimation must always use the real token so
    // the node can price the transaction correctly.
    let final_fee_token = if fee_payer { Address::ZERO } else { fee_token };

    // Optimistic: assume key is already provisioned (no key_authorization).
    let mut key_auth = wallet.signing_mode.key_authorization();
    let mut effective_wallet = wallet;
    // Hold the provisioning-retry signer if we need to rebuild.
    let provisioning_signer;

    let mut gas_request = TempoTransactionRequest {
        calls: calls.clone(),
        key_authorization: key_auth.cloned(),
        ..Default::default()
    }
    .with_fee_token(fee_token)
    .with_nonce_key(EXPIRING_NONCE_KEY);

    if let Some(valid_before) = valid_before.and_then(std::num::NonZeroU64::new) {
        gas_request = gas_request.with_valid_before(valid_before);
    }

    gas_request.inner.from = Some(from);
    gas_request.inner.chain_id = Some(chain_id);
    gas_request.inner.nonce = Some(nonce);
    gas_request.inner.max_fee_per_gas = Some(MAX_FEE_PER_GAS);
    gas_request.inner.max_priority_fee_per_gas = Some(MAX_PRIORITY_FEE_PER_GAS);

    let gas_result = tx_builder::estimate_gas(provider, gas_request).await;

    let gas_limit = match gas_result {
        Ok(gas) => gas,
        Err(original) if wallet.has_stored_key_authorization() => {
            provisioning_signer =
                wallet
                    .with_key_authorization()
                    .ok_or_else(|| KeyError::SigningOperation {
                        operation: "key provisioning",
                        reason: "stored key authorization could not be applied to signing mode"
                            .to_string(),
                    })?;
            effective_wallet = &provisioning_signer;
            key_auth = effective_wallet.signing_mode.key_authorization();
            let mut gas_request = TempoTransactionRequest {
                calls: calls.clone(),
                key_authorization: key_auth.cloned(),
                ..Default::default()
            }
            .with_fee_token(fee_token)
            .with_nonce_key(EXPIRING_NONCE_KEY);

            if let Some(valid_before) = valid_before.and_then(std::num::NonZeroU64::new) {
                gas_request = gas_request.with_valid_before(valid_before);
            }

            gas_request.inner.from = Some(from);
            gas_request.inner.chain_id = Some(chain_id);
            gas_request.inner.nonce = Some(nonce);
            gas_request.inner.max_fee_per_gas = Some(MAX_FEE_PER_GAS);
            gas_request.inner.max_priority_fee_per_gas = Some(MAX_PRIORITY_FEE_PER_GAS);

            tx_builder::estimate_gas(provider, gas_request)
                .await
                .map_err(|source| {
                    classify_tx_error(&source)
                        .or_else(|| classify_tx_error(&original))
                        .unwrap_or_else(|| {
                            KeyError::SigningOperationSource {
                                operation: "estimate gas",
                                source: Box::new(original),
                            }
                            .into()
                        })
                })?
        }
        Err(e) => {
            return Err(classify_tx_error(&e).unwrap_or_else(|| {
                KeyError::SigningOperationSource {
                    operation: "estimate gas",
                    source: Box::new(e),
                }
                .into()
            }))
        }
    };

    let tx = tx_builder::build_tempo_tx(tx_builder::TempoTxOptions {
        calls,
        chain_id,
        fee_token: final_fee_token,
        nonce,
        nonce_key: EXPIRING_NONCE_KEY,
        gas_limit,
        max_fee_per_gas: MAX_FEE_PER_GAS,
        max_priority_fee_per_gas: MAX_PRIORITY_FEE_PER_GAS,
        fee_payer,
        valid_before,
        key_authorization: key_auth.cloned(),
    });

    Ok(
        signing::sign_and_encode_async(
            tx,
            &effective_wallet.signer,
            &effective_wallet.signing_mode,
        )
        .await
        .map_err(|source| KeyError::SigningOperationSource {
            operation: "sign and encode transaction",
            source: Box::new(source),
        })?,
    )
}

/// Submit a Tempo type-0x76 transaction and return the tx hash.
///
/// Uses expiring nonces so no on-chain nonce fetch is needed.
///
/// # Errors
///
/// Returns an error when signing fails or transaction broadcast fails.
pub async fn submit_tempo_tx(
    provider: &alloy::providers::RootProvider<mpp::client::TempoNetwork>,
    wallet: &Signer,
    chain_id: u64,
    fee_token: Address,
    from: Address,
    calls: Vec<tempo_primitives::transaction::Call>,
) -> ChannelResult<String> {
    let tx_bytes =
        resolve_and_sign_tx(provider, wallet, chain_id, fee_token, from, calls.clone()).await?;

    match provider.send_raw_transaction(&tx_bytes).await {
        Ok(pending) => Ok(format!("{:#x}", pending.tx_hash())),
        Err(original) if wallet.has_stored_key_authorization() => {
            let provisioning_signer =
                wallet
                    .with_key_authorization()
                    .ok_or_else(|| KeyError::SigningOperation {
                        operation: "key provisioning",
                        reason: "stored key authorization could not be applied to signing mode"
                            .to_string(),
                    })?;
            let retry_bytes = resolve_and_sign_tx(
                provider,
                &provisioning_signer,
                chain_id,
                fee_token,
                from,
                calls,
            )
            .await?;
            let pending = provider
                .send_raw_transaction(&retry_bytes)
                .await
                .map_err(|source| {
                    classify_tx_error(&source)
                        .or_else(|| classify_tx_error(&original))
                        .unwrap_or_else(|| {
                            NetworkError::RpcSource {
                                operation: "broadcast transaction",
                                source: Box::new(original),
                            }
                            .into()
                        })
                })?;
            Ok(format!("{:#x}", pending.tx_hash()))
        }
        Err(source) => Err(classify_tx_error(&source).unwrap_or_else(|| {
            NetworkError::RpcSource {
                operation: "broadcast transaction",
                source: Box::new(source),
            }
            .into()
        })),
    }
}

// ==================== Transaction Construction ====================

/// Build the escrow open calls: approve + open.
///
/// Constructs a 2-call sequence:
/// 1. `approve(escrow_contract, deposit)` on the token token
/// 2. `IEscrow::open(payee, token, deposit, salt, authorizedSigner)` on the escrow contract
#[must_use]
pub fn build_open_calls(
    token: Address,
    escrow_contract: Address,
    deposit: u128,
    payee: Address,
    salt: B256,
    authorized_signer: Address,
) -> Vec<Call> {
    let approve_data = Bytes::from(
        ITIP20::approveCall {
            spender: escrow_contract,
            amount: U256::from(deposit),
        }
        .abi_encode(),
    );
    let open_data = Bytes::from(
        IEscrow::openCall::new((payee, token, deposit, salt, authorized_signer)).abi_encode(),
    );

    vec![
        Call {
            to: TxKind::Call(token),
            value: U256::ZERO,
            input: approve_data,
        },
        Call {
            to: TxKind::Call(escrow_contract),
            value: U256::ZERO,
            input: open_data,
        },
    ]
}

/// Build the escrow top-up calls: approve + topUp.
#[must_use]
pub fn build_top_up_calls(
    token: Address,
    escrow_contract: Address,
    channel_id: B256,
    additional_deposit: u128,
) -> Vec<Call> {
    let approve_data = Bytes::from(
        ITIP20::approveCall {
            spender: escrow_contract,
            amount: U256::from(additional_deposit),
        }
        .abi_encode(),
    );
    let top_up_data = Bytes::from(
        IEscrow::topUpCall::new((channel_id, U256::from(additional_deposit))).abi_encode(),
    );

    vec![
        Call {
            to: TxKind::Call(token),
            value: U256::ZERO,
            input: approve_data,
        },
        Call {
            to: TxKind::Call(escrow_contract),
            value: U256::ZERO,
            input: top_up_data,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expiring_valid_before_is_future() {
        let vb = expiring_valid_before();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Must be in the future (now < vb <= now + VALID_BEFORE_SECS)
        assert!(vb > now);
        assert!(vb <= now + VALID_BEFORE_SECS);
    }

    #[test]
    fn test_constants_match_mpp_rs() {
        assert_eq!(MAX_FEE_PER_GAS, 41_000_000_000); // 41 gwei
        assert_eq!(MAX_PRIORITY_FEE_PER_GAS, 1_000_000_000); // 1 gwei
        assert_eq!(EXPIRING_NONCE_KEY, U256::MAX);
    }

    #[test]
    fn test_build_top_up_calls_shape() {
        let calls = build_top_up_calls(
            Address::from([0x11; 20]),
            Address::from([0x22; 20]),
            B256::from([0x33; 32]),
            42,
        );
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn test_build_top_up_calls_selector() {
        let calls = build_top_up_calls(
            Address::from([0x11; 20]),
            Address::from([0x22; 20]),
            B256::from([0x33; 32]),
            42,
        );
        // topUp(bytes32,uint256) selector = 0xb67644b9
        let top_up_input = calls[1].input.as_ref();
        assert_eq!(&top_up_input[..4], &[0xb6, 0x76, 0x44, 0xb9]);
    }

    #[test]
    fn test_build_open_calls_selector() {
        let calls = build_open_calls(
            Address::from([0x11; 20]),
            Address::from([0x22; 20]),
            42,
            Address::from([0x33; 20]),
            B256::from([0x44; 32]),
            Address::from([0x55; 20]),
        );
        assert_eq!(calls.len(), 2);
        // approve(address,uint256) selector = 0x095ea7b3
        let approve_input = calls[0].input.as_ref();
        assert_eq!(&approve_input[..4], &[0x09, 0x5e, 0xa7, 0xb3]);
        // open(address,address,uint128,bytes32,address) selector
        let open_input = calls[1].input.as_ref();
        assert!(
            open_input.len() > 4,
            "open call should have ABI-encoded data"
        );
    }
}
