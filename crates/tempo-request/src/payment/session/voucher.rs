//! Voucher credential construction for session payments.

use alloy::primitives::{Address, B256};

use mpp::{
    protocol::methods::tempo::{session::SessionCredentialPayload, sign_voucher},
    ChallengeEcho,
};

use super::ChannelState;
use tempo_common::error::{KeyError, TempoError};

/// Build a `SessionCredentialPayload::Open` with the given transaction bytes.
pub(super) fn build_open_payload(
    channel_id: B256,
    transaction: String,
    authorized_signer: Address,
    descriptor: Option<mpp::protocol::methods::tempo::session::ChannelDescriptor>,
    cumulative_amount: u128,
    voucher_sig: &[u8],
) -> SessionCredentialPayload {
    SessionCredentialPayload::Open {
        payload_type: "transaction".to_string(),
        channel_id: format!("{channel_id:#x}"),
        transaction,
        authorized_signer: Some(format!("{authorized_signer:#x}")),
        descriptor,
        cumulative_amount: cumulative_amount.to_string(),
        signature: format!("0x{}", hex::encode(voucher_sig)),
    }
}

/// Build a voucher credential for an existing session.
pub(super) async fn build_voucher_credential(
    signer: &tempo_common::keys::Signer,
    echo: &ChallengeEcho,
    did: &str,
    state: &ChannelState,
) -> Result<mpp::PaymentCredential, TempoError> {
    let payload = if state.is_tip1034() {
        let descriptor = state
            .descriptor
            .clone()
            .ok_or_else(|| KeyError::SigningOperation {
                operation: "sign TIP-1034 voucher",
                reason: "missing channel descriptor".to_string(),
            })?;
        mpp::client::tempo::session::channel_ops::create_precompile_voucher_payload_with_descriptor_and_escrow(
            &signer.signer,
            descriptor,
            state.cumulative_amount,
            state.escrow_contract,
            state.chain_id,
        )
        .await
        .map_err(|source| KeyError::SigningOperationSource {
            operation: "sign TIP-1034 voucher",
            source: Box::new(source),
        })?
    } else {
        let sig = sign_voucher(
            &signer.signer,
            state.channel_id,
            state.cumulative_amount,
            state.escrow_contract,
            state.chain_id,
        )
        .await
        .map_err(|source| KeyError::SigningOperationSource {
            operation: "sign voucher",
            source: Box::new(source),
        })?;

        SessionCredentialPayload::Voucher {
            channel_id: format!("{:#x}", state.channel_id),
            descriptor: None,
            cumulative_amount: state.cumulative_amount.to_string(),
            signature: format!("0x{}", hex::encode(&sig)),
        }
    };

    Ok(mpp::PaymentCredential::with_source(
        echo.clone(),
        did.to_string(),
        payload,
    ))
}

/// Build a `SessionCredentialPayload::TopUp` from a signed top-up transaction.
pub(super) fn build_top_up_payload(
    channel_id: B256,
    transaction: String,
    descriptor: Option<mpp::protocol::methods::tempo::session::ChannelDescriptor>,
    additional_deposit: u128,
) -> SessionCredentialPayload {
    SessionCredentialPayload::TopUp {
        payload_type: "transaction".to_string(),
        channel_id: format!("{channel_id:#x}"),
        transaction,
        descriptor,
        additional_deposit: additional_deposit.to_string(),
    }
}
