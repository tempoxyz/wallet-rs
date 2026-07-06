//! Transfer tokens to an address.

use alloy::{
    network::ReceiptResponse,
    primitives::{
        address,
        utils::{parse_units, ParseUnits},
        Address, Bytes, TxKind, U256,
    },
    providers::Provider,
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use serde::Serialize;
use tempo_primitives::transaction::Call;

use tempo_common::{
    cli::{context::Context, output, terminal::hyperlink},
    error::{InputError, NetworkError, TempoError},
    payment::session::submit_tempo_tx_and_wait,
};

sol! {
    #[sol(rpc)]
    interface ITIP20 {
        function transfer(address to, uint256 amount) external returns (bool);

        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
    }
}

// ---------------------------------------------------------------------------
// TIP-1028 receive-policy guard (T6)
// ---------------------------------------------------------------------------

/// `ReceivePolicyGuard` precompile address (TIP-1028).
const RECEIVE_POLICY_GUARD_ADDRESS: Address = address!("b10c000000000000000000000000000000000000");

sol! {
    #[derive(Debug, PartialEq, Eq)]
    enum InboundKind {
        TRANSFER,
        MINT,
    }

    /// ABI-encoded claim witness emitted in `TransferBlocked.receipt`.
    #[derive(Debug)]
    struct ClaimReceiptV1 {
        uint8 version;
        address token;
        address recoveryAuthority;
        address originator;
        address recipient;
        uint64 blockedAt;
        uint64 blockedNonce;
        uint8 blockedReason;
        InboundKind kind;
        bytes32 memo;
    }

    /// Emitted by `ReceivePolicyGuard` when an inbound transfer/mint is held.
    event TransferBlocked(
        address indexed token,
        address indexed receiver,
        uint64 indexed blockedNonce,
        uint256 amount,
        uint8 receiptVersion,
        bytes receipt
    );
}

// ---------------------------------------------------------------------------
// Token resolution
// ---------------------------------------------------------------------------

/// A resolved token: address, display symbol, and decimals.
struct ResolvedToken {
    address: Address,
    symbol: String,
    decimals: u8,
}

/// Resolve a `0x`-prefixed token address, querying symbol and decimals on-chain.
///
/// Accepts both `0x…` and `tempox0x…` formats.
async fn resolve_token(input: &str, provider: &impl Provider) -> Result<ResolvedToken, TempoError> {
    let address = tempo_common::security::parse_address_input(input, "token address")?;

    let contract = ITIP20::new(address, provider);

    let decimals = contract
        .decimals()
        .call()
        .await
        .map_err(|source| NetworkError::RpcSource {
            operation: "query token decimals",
            source: Box::new(source),
        })?;

    let symbol = contract
        .symbol()
        .call()
        .await
        .unwrap_or_else(|_| format!("{address:#x}"));

    Ok(ResolvedToken {
        address,
        symbol,
        decimals,
    })
}

// ---------------------------------------------------------------------------
// Amount parsing
// ---------------------------------------------------------------------------

/// Parse a human amount string into atomic units.
///
/// Supports decimal amounts like "1.00" and "50".
fn resolve_amount(input: &str, token: &ResolvedToken) -> Result<(U256, String), TempoError> {
    let parsed = parse_units(input, token.decimals)
        .map_err(|_| InputError::InvalidHexInput(format!("Invalid amount: '{input}'")))?;
    let amount = match parsed {
        ParseUnits::U256(v) => v,
        ParseUnits::I256(v) => {
            if v.is_negative() {
                return Err(
                    InputError::InvalidHexInput("Amount must be positive.".to_string()).into(),
                );
            }
            v.into_raw()
        }
    };

    if amount.is_zero() {
        return Err(
            InputError::InvalidHexInput("Amount must be greater than zero.".to_string()).into(),
        );
    }

    Ok((amount, input.to_string()))
}

// ---------------------------------------------------------------------------
// JSON response
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct TransferResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<String>,
    chain_id: u64,
    amount: String,
    symbol: String,
    token: String,
    to: String,
    from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blockhash: Option<String>,
    /// Present when the transfer was held by the recipient's receive policy (TIP-1028).
    #[serde(skip_serializing_if = "Option::is_none", flatten)]
    held: Option<HeldTransferInfo>,
}

/// Details of a transfer held by a TIP-1028 receive policy.
#[derive(Debug, Serialize)]
struct HeldTransferInfo {
    /// Guard nonce assigned to the blocked transfer, for correlation.
    blocked_nonce: u64,
    /// ABI-encoded receipt witness, usable as the `claim`/`balanceOf` argument.
    claim_receipt: String,
    /// Recovery authority captured in the receipt (`0x0…0` means originator recovery).
    recovery_authority: String,
    /// Who is authorized to claim: `originator`, `recovery_authority`, or `unknown`.
    claimable_by: &'static str,
    /// Whether this wallet (the sender) is the authorized claimer.
    claimable_by_this_wallet: bool,
}

/// A transfer redirected to `ReceivePolicyGuard` by the recipient's receive policy.
#[derive(Debug)]
struct HeldTransfer {
    blocked_nonce: u64,
    recovery_authority: Address,
    /// True when `from` (the sender) may claim the held funds.
    claimable_by_sender: bool,
    receipt_witness: Bytes,
}

/// Classify a confirmed transfer: `Err` if reverted, `Ok(Some)` if held by a
/// receive policy, `Ok(None)` if credited. Split from the receipt so the
/// decision is unit-testable.
fn classify_transfer_outcome(
    status: bool,
    logs: &[alloy::rpc::types::Log],
    token: Address,
    from: Address,
    to: Address,
    amount: U256,
    tx_hash: &str,
) -> Result<Option<HeldTransfer>, TempoError> {
    if !status {
        return Err(NetworkError::Rpc {
            operation: "token transfer",
            reason: format!("transaction reverted (tx {tx_hash})"),
        }
        .into());
    }

    Ok(find_blocked_transfer_in_logs(logs, token, from, to, amount))
}

/// Core log scan for a `TransferBlocked` match, split out for testing.
fn find_blocked_transfer_in_logs(
    logs: &[alloy::rpc::types::Log],
    token: Address,
    from: Address,
    to: Address,
    amount: U256,
) -> Option<HeldTransfer> {
    logs.iter().find_map(|log| {
        if log.address() != RECEIVE_POLICY_GUARD_ADDRESS {
            return None;
        }

        let event = TransferBlocked::decode_log(log.as_ref()).ok()?;
        if event.token != token || event.amount != amount || event.receiptVersion != 1 {
            return None;
        }

        // Decode the v1 witness so virtual-address recipients match correctly.
        let claim = ClaimReceiptV1::abi_decode(&event.receipt).ok()?;
        if claim.version != 1
            || claim.blockedNonce != event.blockedNonce
            || claim.token != token
            || claim.originator != from
            || claim.recipient != to
            || claim.kind != InboundKind::TRANSFER
        {
            return None;
        }

        let claimable_by_sender = if claim.recoveryAuthority == Address::ZERO {
            // Originator recovery: only the originator may claim.
            from == claim.originator
        } else {
            claim.recoveryAuthority == from
        };

        Some(HeldTransfer {
            blocked_nonce: event.blockedNonce,
            recovery_authority: claim.recoveryAuthority,
            claimable_by_sender,
            receipt_witness: event.receipt.clone(),
        })
    })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub(crate) async fn run(
    ctx: &Context,
    amount: String,
    token_input: String,
    to: String,
    fee_token_input: Option<String>,
    dry_run: bool,
) -> Result<(), TempoError> {
    // Ensure wallet is connected
    ctx.keys.ensure_key_for_network(ctx.network)?;

    let wallet = ctx.keys.signer(ctx.network)?;
    let from = wallet.from;

    // Validate recipient address early (no network needed)
    let to_address = tempo_common::security::parse_address_input(&to, "recipient address")?;

    let rpc_url = ctx.config.rpc_url(ctx.network);
    let provider = alloy::providers::ProviderBuilder::new().connect_http(rpc_url.clone());

    // Resolve token
    let token = resolve_token(&token_input, &provider).await?;

    // Resolve amount
    let (amount_atomic, amount_human) = resolve_amount(&amount, &token)?;

    // Resolve fee token (default: same token as transfer)
    let fee_token_address = if let Some(ref ft) = fee_token_input {
        let ft_resolved = resolve_token(ft, &provider).await?;
        ft_resolved.address
    } else {
        token.address
    };

    // Dry run
    if dry_run {
        let response = TransferResponse {
            status: "dry_run",
            tx_hash: None,
            chain_id: ctx.network.chain_id(),
            amount: amount_human,
            symbol: token.symbol,
            token: format!("{:#x}", token.address),
            to: format!("{to_address:#x}"),
            from: format!("{from:#x}"),
            fee: None,
            blockhash: None,
            held: None,
        };

        return output::emit_by_format(ctx.output_format, &response, || {
            eprintln!("[DRY RUN]");
            eprintln!(
                "  Sending {} {} → {}",
                response.amount,
                response.symbol,
                format_address(to_address)
            );
            eprintln!("  From: {}", format_address(from));
            eprintln!("  Fee token: {fee_token_address:#x}");
            Ok(())
        });
    }

    // Build transfer call
    let transfer_data = Bytes::from(
        ITIP20::transferCall {
            to: to_address,
            amount: amount_atomic,
        }
        .abi_encode(),
    );

    let calls = vec![Call {
        to: TxKind::Call(token.address),
        value: U256::ZERO,
        input: transfer_data,
    }];

    // Print pre-confirmation
    if !ctx.output_format.is_structured() {
        eprintln!(
            "  Sending {} {} → {}",
            amount_human,
            token.symbol,
            format_address(to_address)
        );
    }

    let chain_id = ctx.network.chain_id();
    let tempo_provider =
        alloy::providers::RootProvider::<mpp::client::TempoNetwork>::new_http(rpc_url);

    let confirmed = submit_tempo_tx_and_wait(
        &tempo_provider,
        &wallet,
        chain_id,
        fee_token_address,
        from,
        calls,
    )
    .await?;

    let tx_hash = confirmed.tx_hash.clone();
    let tx_url = ctx.network.tx_url(&tx_hash);
    let blockhash = confirmed.receipt.block_hash().map(|h| format!("{h:#x}"));

    // Revert errors; otherwise detect a TIP-1028 (T6) held transfer.
    let held = classify_transfer_outcome(
        confirmed.receipt.status(),
        confirmed.receipt.inner.logs(),
        token.address,
        from,
        to_address,
        amount_atomic,
        &tx_hash,
    )?;

    let response = TransferResponse {
        status: if held.is_some() { "held" } else { "success" },
        tx_hash: Some(tx_hash.clone()),
        chain_id: ctx.network.chain_id(),
        amount: amount_human,
        symbol: token.symbol,
        token: format!("{:#x}", token.address),
        to: format!("{to_address:#x}"),
        from: format!("{from:#x}"),
        fee: None,
        blockhash,
        held: held.as_ref().map(|h| HeldTransferInfo {
            blocked_nonce: h.blocked_nonce,
            claim_receipt: format!("0x{}", hex::encode(&h.receipt_witness)),
            recovery_authority: format!("{:#x}", h.recovery_authority),
            claimable_by: if h.recovery_authority == Address::ZERO {
                "originator"
            } else {
                "recovery_authority"
            },
            claimable_by_this_wallet: h.claimable_by_sender,
        }),
    };

    output::emit_by_format(ctx.output_format, &response, || {
        eprintln!();
        let tx_link = hyperlink(&tx_hash, &tx_url);
        if let Some(h) = &held {
            eprintln!("  Held by recipient's receive policy");
            eprintln!(
                "    Funds were redirected to ReceivePolicyGuard ({RECEIVE_POLICY_GUARD_ADDRESS:#x})"
            );
            eprintln!("    and can be recovered later with the receipt below.");
            eprintln!("    Blocked nonce: {}", h.blocked_nonce);
            if h.recovery_authority == Address::ZERO {
                eprintln!("    Claim authority: originator (this wallet)");
            } else {
                eprintln!(
                    "    Claim authority: {}{}",
                    format_address(h.recovery_authority),
                    if h.claimable_by_sender {
                        " (this wallet)"
                    } else {
                        ""
                    }
                );
            }
            eprintln!("    Claim receipt: 0x{}", hex::encode(&h.receipt_witness));
        } else {
            eprintln!("  Success");
        }
        eprintln!("    TX: {tx_link}");
        if tx_link == tx_hash {
            eprintln!("    {tx_url}");
        }
        Ok(())
    })
}

fn format_address(addr: Address) -> String {
    let s = format!("{addr:#x}");
    if s.len() > 12 {
        format!("{}…{}", &s[..6], &s[s.len() - 4..])
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::{
        primitives::{Bytes, FixedBytes, Log as PrimitiveLog, LogData},
        rpc::types::Log as RpcLog,
    };

    const GUARD: Address = RECEIVE_POLICY_GUARD_ADDRESS;
    const TOKEN: Address = Address::new([0x11; 20]);
    const FROM: Address = Address::new([0x22; 20]);
    const TO: Address = Address::new([0x33; 20]);

    fn rpc_log(address: Address, data: LogData) -> RpcLog {
        RpcLog {
            inner: PrimitiveLog { address, data },
            ..Default::default()
        }
    }

    /// Builder for a `TransferBlocked` guard log, with every field independently
    /// settable so both happy-path and inconsistent receipts can be exercised.
    struct BlockedLog {
        guard: Address,
        token: Address,
        receiver: Address,
        originator: Address,
        recipient: Address,
        recovery: Address,
        amount: U256,
        event_nonce: u64,
        claim_nonce: u64,
        event_version: u8,
        claim_version: u8,
        kind: InboundKind,
    }

    impl BlockedLog {
        /// A fully consistent, matching held transfer of `amount` to `TO`.
        fn matching(amount: U256, nonce: u64) -> Self {
            Self {
                guard: GUARD,
                token: TOKEN,
                receiver: TO,
                originator: FROM,
                recipient: TO,
                recovery: Address::ZERO,
                amount,
                event_nonce: nonce,
                claim_nonce: nonce,
                event_version: 1,
                claim_version: 1,
                kind: InboundKind::TRANSFER,
            }
        }

        fn build(&self) -> RpcLog {
            let claim = ClaimReceiptV1 {
                version: self.claim_version,
                token: self.token,
                recoveryAuthority: self.recovery,
                originator: self.originator,
                recipient: self.recipient,
                blockedAt: 0,
                blockedNonce: self.claim_nonce,
                blockedReason: 0,
                kind: self.kind,
                memo: FixedBytes::ZERO,
            };
            let event = TransferBlocked {
                token: self.token,
                receiver: self.receiver,
                blockedNonce: self.event_nonce,
                amount: self.amount,
                receiptVersion: self.event_version,
                receipt: Bytes::from(claim.abi_encode()),
            };
            rpc_log(self.guard, event.encode_log_data())
        }
    }

    fn find(logs: &[RpcLog], amount: U256) -> Option<HeldTransfer> {
        find_blocked_transfer_in_logs(logs, TOKEN, FROM, TO, amount)
    }

    #[test]
    fn detects_matching_held_transfer() {
        let amount = U256::from(1_000_000u64);
        let held = find(&[BlockedLog::matching(amount, 7).build()], amount)
            .expect("should detect held transfer");
        assert_eq!(held.blocked_nonce, 7);
        assert_eq!(held.recovery_authority, Address::ZERO);
        // Originator recovery and the sender is the originator → claimable by sender.
        assert!(held.claimable_by_sender);
    }

    #[test]
    fn matches_virtual_address_via_receipt_recipient() {
        // Virtual: event.receiver is the resolved master (!= TO), but the witness
        // recipient is the literal TO the sender addressed. Must still match.
        let amount = U256::from(42u64);
        let mut log = BlockedLog::matching(amount, 3);
        log.receiver = Address::new([0x99; 20]);
        let held = find(&[log.build()], amount).expect("should match on receipt recipient");
        assert_eq!(held.blocked_nonce, 3);
    }

    #[test]
    fn non_sender_recovery_authority_is_not_claimable() {
        let amount = U256::from(5u64);
        let recovery = Address::new([0x44; 20]);
        let mut log = BlockedLog::matching(amount, 1);
        log.recovery = recovery;
        let held = find(&[log.build()], amount).unwrap();
        assert_eq!(held.recovery_authority, recovery);
        assert!(!held.claimable_by_sender);
    }

    #[test]
    fn ignores_non_matching_logs() {
        let amount = U256::from(10u64);

        // Spoofed (non-guard) emitter address.
        let mut spoof = BlockedLog::matching(amount, 1);
        spoof.guard = Address::new([0xEE; 20]);

        // Wrong token.
        let mut wrong_token = BlockedLog::matching(amount, 1);
        wrong_token.token = Address::new([0xAB; 20]);

        // Wrong amount.
        let wrong_amount = BlockedLog::matching(U256::from(999u64), 1);

        // Blocked mint, not a transfer.
        let mut mint = BlockedLog::matching(amount, 1);
        mint.kind = InboundKind::MINT;

        // Unrelated event at the guard address (e.g. the normal Transfer → guard).
        let junk = rpc_log(
            GUARD,
            LogData::new_unchecked(vec![FixedBytes::with_last_byte(0xAB)], Bytes::new()),
        );

        for log in [
            spoof.build(),
            wrong_token.build(),
            wrong_amount.build(),
            mint.build(),
            junk,
        ] {
            assert!(find(&[log], amount).is_none());
        }
    }

    #[test]
    fn rejects_inconsistent_receipt() {
        let amount = U256::from(10u64);

        let mut bad_claim_version = BlockedLog::matching(amount, 1);
        bad_claim_version.claim_version = 2;

        let mut bad_event_version = BlockedLog::matching(amount, 1);
        bad_event_version.event_version = 2;

        // claim/event nonce disagree
        let mut nonce_mismatch = BlockedLog::matching(amount, 9);
        nonce_mismatch.claim_nonce = 5;

        for log in [
            bad_claim_version.build(),
            bad_event_version.build(),
            nonce_mismatch.build(),
        ] {
            assert!(find(&[log], amount).is_none());
        }
    }

    fn status_of(outcome: &Result<Option<HeldTransfer>, TempoError>) -> &'static str {
        match outcome {
            Ok(Some(_)) => "held",
            Ok(None) => "success",
            Err(_) => "error",
        }
    }

    #[test]
    fn classify_reverted_receipt_errors() {
        let amount = U256::from(10u64);
        let outcome = classify_transfer_outcome(false, &[], TOKEN, FROM, TO, amount, "0xdead");
        assert!(outcome.is_err());
        assert!(outcome.unwrap_err().to_string().contains("reverted"));
    }

    #[test]
    fn classify_blocked_receipt_is_held() {
        let amount = U256::from(10u64);
        let logs = [BlockedLog::matching(amount, 7).build()];
        let outcome = classify_transfer_outcome(true, &logs, TOKEN, FROM, TO, amount, "0xabc");
        assert_eq!(status_of(&outcome), "held");
        assert_eq!(outcome.unwrap().unwrap().blocked_nonce, 7);
    }

    #[test]
    fn classify_credited_receipt_is_success() {
        let amount = U256::from(10u64);
        let outcome = classify_transfer_outcome(true, &[], TOKEN, FROM, TO, amount, "0xabc");
        assert_eq!(status_of(&outcome), "success");
        assert!(outcome.unwrap().is_none());
    }

    #[test]
    fn response_json_shape() {
        // Success: no held fields leak into the output.
        let success = TransferResponse {
            status: "success",
            tx_hash: Some("0xabc".to_string()),
            chain_id: 42431,
            amount: "1.00".to_string(),
            symbol: "USDC".to_string(),
            token: "0x11".to_string(),
            to: "0x33".to_string(),
            from: "0x22".to_string(),
            fee: None,
            blockhash: Some("0xdef".to_string()),
            held: None,
        };
        let json = serde_json::to_value(&success).unwrap();
        assert_eq!(json["status"], "success");
        assert!(json.get("blocked_nonce").is_none());
        assert!(json.get("held").is_none());

        // Held: HeldTransferInfo is flattened to top level, not nested under "held".
        let held = TransferResponse {
            status: "held",
            held: Some(HeldTransferInfo {
                blocked_nonce: 7,
                claim_receipt: "0xdeadbeef".to_string(),
                recovery_authority: "0x0".to_string(),
                claimable_by: "originator",
                claimable_by_this_wallet: true,
            }),
            ..success
        };
        let json = serde_json::to_value(&held).unwrap();
        assert_eq!(json["status"], "held");
        assert_eq!(json["blocked_nonce"], 7);
        assert_eq!(json["claim_receipt"], "0xdeadbeef");
        assert_eq!(json["claimable_by"], "originator");
        assert_eq!(json["claimable_by_this_wallet"], true);
        assert!(json.get("held").is_none());
    }
}
