use std::collections::{HashMap, HashSet};

use alloy::primitives::Address;

use super::{
    render::{render_channel_list, ChannelView},
    session, ChannelStatus,
};
use tempo_common::{
    cli::context::Context,
    error::TempoError,
    payment::session::{find_all_channels_for_payer, DiscoveredChannel},
};

/// List payment sessions.
///
/// By default lists all non-finalized local sessions from the DB and does not
/// perform network calls. With `--orphaned` (or `--all`) it scans on-chain for
/// channels without a local session record, persists discovered channels to the
/// local DB, and includes them in output.
pub(super) async fn list_channels(
    ctx: &Context,
    orphaned: bool,
    all: bool,
) -> Result<(), TempoError> {
    let config = &ctx.config;
    let output_format = ctx.output_format;
    let network = ctx.network;
    let keys = &ctx.keys;
    let now = session::now_secs();

    let orphaned_only = orphaned && !all;
    let include_orphaned_discovery = orphaned || all;

    // Local sessions (DB only).
    let sessions = session::list_channels()?;
    let filtered_local: Vec<_> = sessions
        .iter()
        .filter(|s| s.network_id() == network)
        .collect();

    let mut views: Vec<ChannelView> = Vec::new();

    // Build local views.
    for s in &filtered_local {
        let (session_status, _) = s.status_at(now);
        if matches!(session_status, ChannelStatus::Finalized) {
            continue;
        }
        if orphaned_only && !matches!(session_status, ChannelStatus::Orphaned) {
            continue;
        }
        let v = ChannelView::from(*s);
        views.push(v);
    }

    // On-chain orphaned discovery is opt-in only.
    if let Some(wallet_addr) = include_orphaned_discovery
        .then(|| keys.wallet_address_parsed())
        .flatten()
    {
        let channels = find_all_channels_for_payer(config, wallet_addr, network).await;

        // Avoid duplicates by skipping any with a local session
        let local_ids: HashSet<String> =
            filtered_local.iter().map(|s| s.channel_id_hex()).collect();

        // Cache grace per escrow to reduce RPC chatter
        let mut grace_cache: HashMap<Address, u64> = HashMap::new();

        for ch in &channels {
            let channel_id_hex = format!("{:#x}", ch.channel_id);
            if local_ids.contains(&channel_id_hex.to_lowercase()) {
                continue;
            }
            let grace = if let Some(&g) = grace_cache.get(&ch.escrow_contract) {
                g
            } else {
                let g =
                    super::util::resolve_grace_period(config, ch.network, ch.escrow_contract).await;
                grace_cache.insert(ch.escrow_contract, g);
                g
            };

            let mut v = ChannelView::from_on_chain(
                &channel_id_hex,
                network,
                ch.deposit,
                ch.settled,
                ch.close_requested_at,
                grace,
            );
            persist_discovered_channel(ch, wallet_addr, grace)?;
            // Show the "Channel" line in text output (origin presence triggers it)
            v.origin = Some(String::new());
            views.push(v);
        }
    }

    let empty_msg = if orphaned_only {
        "No orphaned sessions found."
    } else {
        "No sessions found."
    };

    render_channel_list(&views, output_format, empty_msg, "session(s) total")
}

fn persist_discovered_channel(
    ch: &DiscoveredChannel,
    wallet_addr: Address,
    grace_period: u64,
) -> Result<(), TempoError> {
    let now = session::now_secs();
    let grace_ready_at = super::util::grace_ready_at(ch.close_requested_at, grace_period);
    let state = super::util::status_from_close_timing(ch.close_requested_at, grace_period, now);

    let record = session::ChannelRecord {
        version: 1,
        origin: String::new(),
        request_url: String::new(),
        chain_id: ch.network.chain_id(),
        escrow_contract: ch.escrow_contract,
        token: format!("{:#x}", ch.token),
        payee: format!("{:#x}", Address::ZERO),
        payer: format!("{wallet_addr:#x}"),
        authorized_signer: Address::ZERO,
        salt: "0x00".to_string(),
        channel_id: ch.channel_id,
        session_protocol: mpp::protocol::methods::tempo::session::SESSION_PROTOCOL_LEGACY
            .to_string(),
        descriptor_json: None,
        deposit: ch.deposit,
        cumulative_amount: ch.settled,
        accepted_cumulative: ch.settled,
        server_spent: 0,
        challenge_echo: "{}".to_string(),
        state,
        close_requested_at: ch.close_requested_at,
        grace_ready_at,
        created_at: now,
        last_used_at: now,
    };

    session::save_channel(&record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_common::session::DEFAULT_GRACE_PERIOD_SECS;

    fn make_record(
        state: ChannelStatus,
        grace_ready_at: u64,
        last_used_at: u64,
    ) -> session::ChannelRecord {
        session::ChannelRecord {
            version: 1,
            origin: "https://api.example.com".into(),
            request_url: "https://api.example.com/v1".into(),
            chain_id: 4217,
            escrow_contract: Address::ZERO,
            token: "0x00".into(),
            payee: "0x00".into(),
            payer: "did:pkh:eip155:4217:0x00".into(),
            authorized_signer: Address::ZERO,
            salt: "0x00".into(),
            channel_id: "0x0000000000000000000000000000000000000000000000000000000000000abc"
                .parse()
                .unwrap(),
            session_protocol: mpp::protocol::methods::tempo::session::SESSION_PROTOCOL_LEGACY
                .to_string(),
            descriptor_json: None,
            deposit: 1_000_000,
            cumulative_amount: 2_000,
            accepted_cumulative: 0,
            server_spent: 0,
            challenge_echo: "{}".into(),
            state,
            close_requested_at: if state == ChannelStatus::Closing {
                grace_ready_at.saturating_sub(DEFAULT_GRACE_PERIOD_SECS)
            } else {
                0
            },
            grace_ready_at,
            created_at: last_used_at.saturating_sub(60),
            last_used_at,
        }
    }

    #[test]
    fn test_view_from_session_active() {
        let now = session::now_secs();
        let rec = make_record(ChannelStatus::Active, 0, now);
        let view = ChannelView::from(&rec);
        assert_eq!(view.status, ChannelStatus::Active);
        assert!(view.remaining_secs.is_none());
    }

    #[test]
    fn test_view_from_session_closing_and_finalizable() {
        let now = session::now_secs();
        // Closing with time remaining
        let rec = make_record(ChannelStatus::Closing, now + 120, now);
        let view = ChannelView::from(&rec);
        assert_eq!(view.status, ChannelStatus::Closing);
        assert_eq!(view.remaining_secs, Some(120));

        // Finalizable (ready_at <= now)
        let rec2 = make_record(ChannelStatus::Closing, now, now);
        let view2 = ChannelView::from(&rec2);
        assert_eq!(view2.status, ChannelStatus::Finalizable);
        assert_eq!(view2.remaining_secs, Some(0));
    }
}
