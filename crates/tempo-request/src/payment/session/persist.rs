//! Session persistence helper for request-time session flows.

use alloy::primitives::B256;

use tempo_common::error::{PaymentError, TempoError};

use tempo_common::session::{
    load_channel, now_secs, save_channel, update_channel_cumulative_floor, ChannelRecord,
    ChannelStatus,
};

use super::{ChannelContext, ChannelState};

/// Persist or update the session record to disk.
pub(super) fn persist_session(
    ctx: &ChannelContext<'_>,
    state: &ChannelState,
) -> Result<(), TempoError> {
    let now = now_secs();

    let echo_json = serde_json::to_string(ctx.echo).map_err(|source| {
        PaymentError::ChannelPersistenceSource {
            operation: "serialize challenge echo",
            source: Box::new(source),
        }
    })?;

    let existing = load_channel(&format!("{:#x}", state.channel_id)).map_err(|source| {
        PaymentError::ChannelPersistenceSource {
            operation: "load channel",
            source: Box::new(source),
        }
    })?;

    let descriptor_json = state
        .descriptor
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|source| PaymentError::ChannelPersistenceSource {
            operation: "serialize channel descriptor",
            source: Box::new(source),
        })?;

    let record = if let Some(mut rec) = existing {
        // Update existing record
        rec.set_cumulative_amount(state.cumulative_amount);
        rec.set_accepted_cumulative(state.accepted_cumulative);
        if state.server_spent > 0 {
            rec.set_server_spent(state.server_spent);
        }
        rec.deposit = state.deposit;
        rec.challenge_echo = echo_json;
        rec.session_protocol = state.session_protocol.clone();
        rec.descriptor_json = descriptor_json;
        rec.touch();
        rec
    } else {
        ChannelRecord {
            version: 1,
            origin: ctx.origin.to_string(),
            request_url: ctx.url.to_string(),
            chain_id: state.chain_id,
            escrow_contract: state.escrow_contract,
            token: format!("{:#x}", ctx.token),
            payee: format!("{:#x}", ctx.payee),
            payer: format!("{:#x}", ctx.payer),
            authorized_signer: ctx.signer.signer.address(),
            salt: ctx.salt.clone(),
            channel_id: state.channel_id,
            session_protocol: state.session_protocol.clone(),
            descriptor_json,
            deposit: state.deposit,
            cumulative_amount: state.cumulative_amount,
            accepted_cumulative: state.accepted_cumulative,
            server_spent: state.server_spent,
            challenge_echo: echo_json,
            state: ChannelStatus::Active,
            close_requested_at: 0,
            grace_ready_at: 0,
            created_at: now,
            last_used_at: now,
        }
    };

    save_channel(&record).map_err(|source| PaymentError::ChannelPersistenceSource {
        operation: "save channel",
        source: Box::new(source),
    })?;

    if ctx.http.debug_enabled() {
        let cumulative_display =
            tempo_common::cli::format::format_token_amount(state.cumulative_amount, ctx.network_id);
        eprintln!("Channel persisted (cumulative: {cumulative_display})");
    }

    Ok(())
}

pub(super) fn persist_channel_cumulative_floor(
    channel_id: B256,
    accepted_cumulative: u128,
) -> Result<(), TempoError> {
    let channel_id_hex = format!("{channel_id:#x}");
    update_channel_cumulative_floor(&channel_id_hex, accepted_cumulative).map_err(|source| {
        PaymentError::ChannelPersistenceSource {
            operation: "update channel cumulative floor",
            source: Box::new(source),
        }
    })?;
    Ok(())
}
