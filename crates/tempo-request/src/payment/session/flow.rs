use std::error::Error;

use alloy::primitives::{utils::parse_units, Address, B256};
use mpp::protocol::methods::tempo::{compute_channel_id, session::TempoSessionExt, sign_voucher};

use super::{
    error_map::payment_rejected_from_body,
    extract_origin, new_idempotency_key, open,
    persist::persist_session,
    receipt::{
        apply_receipt_amounts_strict, invalid_payment_receipt_error, missing_payment_receipt_error,
        parse_validated_session_receipt_header, protocol_spent_error,
    },
    streaming,
    voucher::{build_open_payload, build_top_up_payload, build_voucher_credential},
    ChannelContext, ChannelState,
};
use crate::{
    http::HttpResponse,
    payment::{
        challenge::{decode_session_request, require_session_chain_id},
        lock::{acquire_origin_lock, origin_lock_key},
        types::{PaymentResult, ResolvedChallenge},
    },
};
use tempo_common::{
    cli::{
        format::format_token_amount,
        terminal::{address_link, sanitize_for_terminal},
    },
    error::{KeyError, NetworkError, PaymentError, TempoError},
    keys::{Keystore, Signer},
    payment::{
        classify::{map_mpp_validation_error, parse_problem_details, SessionProblemType},
        session,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionRequestFailureKind {
    ChannelInvalidated,
    Other,
}

struct SessionRequestFailure {
    source: TempoError,
    kind: SessionRequestFailureKind,
}

impl SessionRequestFailure {
    fn into_tempo(self) -> TempoError {
        self.source
    }
}

fn session_store_error<E>(operation: &'static str, source: E) -> TempoError
where
    E: Error + Send + Sync + 'static,
{
    PaymentError::ChannelPersistenceSource {
        operation,
        source: Box::new(source),
    }
    .into()
}

fn session_reuse_preserved_error(source: TempoError) -> TempoError {
    PaymentError::ChannelPersistenceContextSource {
        operation: "session request reuse",
        context: "Session request failed; channel state preserved for on-chain dispute",
        source: Box::new(source),
    }
    .into()
}

fn session_reuse_persist_failed_error(source: TempoError) -> TempoError {
    PaymentError::ChannelPersistenceContextSource {
        operation: "session request reuse",
        context: "Failed to persist preserved channel state for on-chain dispute",
        source: Box::new(source),
    }
    .into()
}

fn session_reuse_cleanup_failed_error(source: TempoError) -> TempoError {
    PaymentError::ChannelPersistenceContextSource {
        operation: "session request reuse",
        context: "Failed to remove invalidated channel before reopening",
        source: Box::new(source),
    }
    .into()
}

const fn challenge_missing_field(context: &'static str, field: &'static str) -> PaymentError {
    PaymentError::ChallengeMissingField { context, field }
}

fn challenge_address_parse(
    context: &'static str,
    source: alloy::hex::FromHexError,
) -> PaymentError {
    PaymentError::ChallengeAddressParse {
        context,
        source: Box::new(source),
    }
}

fn challenge_u128_parse(value: &str, context: &'static str) -> Result<u128, TempoError> {
    value.parse::<u128>().map_err(|source| {
        PaymentError::ChallengeValueParse {
            context,
            source: Box::new(source),
        }
        .into()
    })
}

fn parse_max_spend(
    raw_max_spend: Option<&str>,
    network_id: tempo_common::network::NetworkId,
) -> Result<Option<u128>, TempoError> {
    let Some(raw) = raw_max_spend else {
        return Ok(None);
    };

    let parsed = parse_units(raw, network_id.token().decimals).map_err(|_| {
        PaymentError::ChallengeParse {
            context: "--max-spend",
            reason: format!(
                "invalid amount '{}' (expected decimal token amount)",
                sanitize_for_terminal(raw)
            ),
        }
    })?;

    let amount: u128 = parsed.get_absolute().to();
    if amount == 0 {
        return Err(PaymentError::ChallengeSchema {
            context: "--max-spend",
            reason: "must be greater than 0".to_string(),
        }
        .into());
    }

    Ok(Some(amount))
}

pub(super) fn validate_request_spend_limit(
    state: &ChannelState,
    network_id: tempo_common::network::NetworkId,
    required_cumulative: u128,
) -> Result<(), TempoError> {
    let Some(max_cumulative_spend) = state.max_cumulative_spend else {
        return Ok(());
    };

    if required_cumulative <= max_cumulative_spend {
        return Ok(());
    }

    Err(PaymentError::PaymentRejected {
        reason: format!(
            "Payment max spend exceeded: max={} required={}",
            format_token_amount(max_cumulative_spend, network_id),
            format_token_amount(required_cumulative, network_id),
        ),
        status_code: 402,
    }
    .into())
}

fn normalize_hex_identifier(value: &str) -> String {
    if let Ok(address) = value.parse::<Address>() {
        return format!("{address:#x}");
    }
    if let Ok(channel_id) = value.parse::<B256>() {
        return format!("{channel_id:#x}");
    }

    let trimmed = value.trim();
    if let Some(hex) = trimmed.strip_prefix("0x") {
        return format!("0x{}", hex.to_ascii_lowercase());
    }
    if let Some(hex) = trimmed.strip_prefix("0X") {
        return format!("0x{}", hex.to_ascii_lowercase());
    }
    trimmed.to_ascii_lowercase()
}

fn classify_session_failure(status_code: u16, body: &str) -> SessionRequestFailureKind {
    if status_code == 410 {
        if let Some(problem) = parse_problem_details(body) {
            if matches!(
                problem.classify(),
                SessionProblemType::ChannelNotFound | SessionProblemType::ChannelFinalized
            ) {
                return SessionRequestFailureKind::ChannelInvalidated;
            }
        }
    }

    SessionRequestFailureKind::Other
}

fn invalidation_confirmed_on_chain(on_chain: Option<&session::OnChainChannel>) -> bool {
    match on_chain {
        None => true,
        Some(channel) => channel.close_requested_at != 0,
    }
}

async fn confirm_server_invalidation_on_chain(
    provider: &alloy::providers::RootProvider<alloy::network::Ethereum>,
    escrow_contract: Address,
    channel_id: B256,
) -> Result<bool, TempoError> {
    for _ in 0..2 {
        let on_chain = session::get_channel_on_chain(provider, escrow_contract, channel_id).await?;
        if invalidation_confirmed_on_chain(on_chain.as_ref()) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn unconfirmed_invalidation_error(channel_id: B256) -> TempoError {
    PaymentError::ChannelPersistence {
        operation: "session request reuse",
        reason: format!(
            "Session invalidation claim for channel {channel_id:#x} was not confirmed on-chain"
        ),
    }
    .into()
}

fn challenge_channel_id_parse(value: &str, context: &'static str) -> Result<B256, TempoError> {
    value.parse::<B256>().map_err(|_| {
        let safe_value = sanitize_for_terminal(value);
        PaymentError::ChallengeParse {
            context,
            reason: format!("invalid channelId bytes32 value: {safe_value}"),
        }
        .into()
    })
}

fn assess_on_chain_reusability(channel: &session::OnChainChannel, amount: u128) -> Option<u128> {
    if channel.close_requested_at != 0 {
        return None;
    }

    let available_balance = channel.deposit.saturating_sub(channel.settled);
    if available_balance < amount {
        return None;
    }

    Some(available_balance)
}

async fn validate_reusable_channel_on_chain(
    provider: &alloy::providers::RootProvider<alloy::network::Ethereum>,
    escrow_contract: Address,
    channel_id: B256,
    amount: u128,
) -> Result<Option<ChannelOnChainAvailability>, TempoError> {
    let Some(on_chain) =
        session::get_channel_on_chain(provider, escrow_contract, channel_id).await?
    else {
        return Ok(None);
    };

    if assess_on_chain_reusability(&on_chain, amount).is_none() {
        return Ok(None);
    };

    Ok(Some(ChannelOnChainAvailability { on_chain }))
}

struct ChannelOnChainAvailability {
    on_chain: session::OnChainChannel,
}

fn is_on_chain_identity_match(
    on_chain: &session::OnChainChannel,
    payer: Address,
    payee: Address,
    token: Address,
    authorized_signer: Address,
) -> bool {
    on_chain.payer == payer
        && on_chain.payee == payee
        && on_chain.token == token
        && on_chain.authorized_signer == authorized_signer
}

fn apply_response_receipt(
    response: &HttpResponse,
    state: &mut ChannelState,
    context: &str,
) -> Result<Option<String>, TempoError> {
    if !(200..=299).contains(&response.status_code) {
        return Ok(None);
    }

    let Some(receipt_header) = response.header("payment-receipt") else {
        return Err(missing_payment_receipt_error(context));
    };

    match parse_validated_session_receipt_header(receipt_header, state.channel_id) {
        Ok(receipt) => {
            if let Some(reason) = receipt.spent_parse_error {
                return Err(protocol_spent_error(reason));
            }
            apply_receipt_amounts_strict(state, receipt.accepted_cumulative, receipt.server_spent)?;
            Ok(receipt.tx_reference)
        }
        Err(reason) => Err(invalid_payment_receipt_error(context, &reason)),
    }
}

/// Variant of `apply_response_receipt` that reads headers from a raw
/// `reqwest::Response` without consuming the body.
fn apply_response_receipt_from_headers(
    response: &reqwest::Response,
    state: &mut ChannelState,
    context: &str,
    require_header: bool,
) -> Result<Option<String>, TempoError> {
    if !response.status().is_success() {
        return Ok(None);
    }

    let Some(receipt_header) = response
        .headers()
        .get("payment-receipt")
        .and_then(|v| v.to_str().ok())
    else {
        if require_header {
            return Err(missing_payment_receipt_error(context));
        }
        return Ok(None);
    };

    match parse_validated_session_receipt_header(receipt_header, state.channel_id) {
        Ok(receipt) => {
            if let Some(reason) = receipt.spent_parse_error {
                return Err(protocol_spent_error(reason));
            }
            apply_receipt_amounts_strict(state, receipt.accepted_cumulative, receipt.server_spent)?;
            Ok(receipt.tx_reference)
        }
        Err(reason) => Err(invalid_payment_receipt_error(context, &reason)),
    }
}

fn parse_positive_problem_amount(value: &str, context: &'static str) -> Result<u128, TempoError> {
    let safe_value = sanitize_for_terminal(value);
    let amount = value
        .parse::<u128>()
        .map_err(|_| PaymentError::ChallengeParse {
            context,
            reason: format!("must be a positive integer amount (got '{safe_value}')"),
        })?;
    if amount == 0 {
        return Err(PaymentError::ChallengeSchema {
            context,
            reason: "must be > 0".to_string(),
        }
        .into());
    }
    Ok(amount)
}

fn validate_problem_channel_id(
    problem: &tempo_common::payment::ProblemDetails,
    expected: B256,
) -> Result<(), TempoError> {
    let Some(value) = problem.channel_id.as_deref() else {
        return Ok(());
    };
    let parsed = challenge_channel_id_parse(value, "session Problem Details channelId")?;
    if parsed != expected {
        return Err(PaymentError::ChallengeSchema {
            context: "session Problem Details channelId",
            reason: format!("channelId mismatch (expected {expected:#x}, got {parsed:#x})"),
        }
        .into());
    }
    Ok(())
}

async fn fetch_fresh_session_echo(
    ctx: &ChannelContext<'_>,
) -> Result<mpp::ChallengeEcho, TempoError> {
    let response = ctx
        .reqwest_client
        .head(ctx.url)
        .send()
        .await
        .map_err(NetworkError::Reqwest)?;

    if response.status().as_u16() != 402 {
        return Err(PaymentError::PaymentRejected {
            reason: format!(
                "Expected 402 while refreshing challenge, got HTTP {}",
                response.status().as_u16()
            ),
            status_code: response.status().as_u16(),
        }
        .into());
    }

    let challenge_header = response
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| PaymentError::MissingHeader("WWW-Authenticate".to_string()))?;

    let challenge = mpp::parse_www_authenticate(challenge_header).map_err(|source| {
        PaymentError::ChallengeParseSource {
            context: "WWW-Authenticate header",
            source: Box::new(source),
        }
    })?;

    challenge
        .validate_for_session("tempo")
        .map_err(|err| map_mpp_validation_error(err, &challenge))?;

    Ok(challenge.to_echo())
}

async fn send_top_up_request(
    ctx: &ChannelContext<'_>,
    echo: &mpp::ChallengeEcho,
    state: &ChannelState,
    additional_deposit: u128,
    idempotency_key: &str,
) -> Result<HttpResponse, TempoError> {
    let calls = session::build_top_up_calls(
        ctx.token,
        state.escrow_contract,
        state.channel_id,
        additional_deposit,
    );

    let payment = open::create_tempo_payment_from_calls(
        ctx.rpc_url,
        ctx.signer,
        calls,
        ctx.token,
        state.chain_id,
        ctx.fee_payer,
    )
    .await?;
    let tx_hex = format!("0x{}", hex::encode(&payment.tx_bytes));
    let payload = build_top_up_payload(state.channel_id, tx_hex, additional_deposit);

    let credential =
        mpp::PaymentCredential::with_source(echo.clone(), ctx.did.to_string(), payload);
    let auth_header = mpp::format_authorization(&credential).map_err(|source| {
        PaymentError::ChallengeFormatSource {
            context: "topUp credential",
            source: Box::new(source),
        }
    })?;

    let response = ctx
        .reqwest_client
        .post(ctx.url)
        .header("Authorization", auth_header)
        .header("Idempotency-Key", idempotency_key)
        .send()
        .await
        .map_err(NetworkError::Reqwest)?;

    HttpResponse::from_reqwest(response).await
}

async fn run_non_streaming_top_up_recovery(
    ctx: &ChannelContext<'_>,
    state: &mut ChannelState,
    additional_deposit: u128,
) -> Result<(), TempoError> {
    if let Some(clamped) = ctx.clamped_deposit {
        if ctx.http.log_enabled() {
            eprintln!(
                "Clamping deposit to 50% of wallet balance: {}",
                format_token_amount(clamped, ctx.network_id)
            );
        }
    }

    let mut echo = fetch_fresh_session_echo(ctx)
        .await
        .unwrap_or_else(|_| ctx.echo.clone());
    let top_up_idempotency_key = new_idempotency_key();

    for retry in 0..=1 {
        let top_up_response = send_top_up_request(
            ctx,
            &echo,
            state,
            additional_deposit,
            &top_up_idempotency_key,
        )
        .await?;
        if top_up_response.status_code < 400 {
            let _ = apply_response_receipt(&top_up_response, state, "topUp response")?;
            state.deposit = state.deposit.saturating_add(additional_deposit);
            persist_session(ctx, state)?;
            if ctx.http.log_enabled() {
                eprintln!(
                    "Topped up channel {:#x} (+{}, new deposit: {})",
                    state.channel_id,
                    format_token_amount(additional_deposit, ctx.network_id),
                    format_token_amount(state.deposit, ctx.network_id),
                );
            }
            return Ok(());
        }

        let body = top_up_response.body_string().unwrap_or_default();
        if retry == 0
            && top_up_response.status_code == 410
            && parse_problem_details(&body)
                .is_some_and(|problem| problem.classify() == SessionProblemType::ChallengeNotFound)
        {
            echo = fetch_fresh_session_echo(ctx).await?;
            continue;
        }

        return Err(payment_rejected_from_body(
            top_up_response.status_code,
            &body,
        ));
    }

    Err(PaymentError::PaymentRejected {
        reason: "Top-up retry exhausted".to_string(),
        status_code: 410,
    }
    .into())
}

/// Send the actual session request with a voucher and handle the response.
///
/// Bypasses [`crate::http::HttpClient::execute()`] and uses the raw reqwest client directly
/// because session streaming requires access to `reqwest::Response` for SSE
/// `bytes_stream()`, which `execute()` does not expose.
async fn send_session_request(
    ctx: &ChannelContext<'_>,
    state: &mut ChannelState,
) -> Result<PaymentResult, SessionRequestFailure> {
    validate_request_spend_limit(state, ctx.network_id, state.cumulative_amount).map_err(
        |source| SessionRequestFailure {
            source,
            kind: SessionRequestFailureKind::Other,
        },
    )?;

    let mut attempted_non_stream_top_up = false;
    let request_idempotency_key = new_idempotency_key();
    loop {
        let voucher_credential = build_voucher_credential(ctx.signer, ctx.echo, ctx.did, state)
            .await
            .map_err(|source| SessionRequestFailure {
                source,
                kind: SessionRequestFailureKind::Other,
            })?;

        let voucher_auth = mpp::format_authorization(&voucher_credential).map_err(|source| {
            SessionRequestFailure {
                source: PaymentError::ChallengeFormatSource {
                    context: "voucher credential",
                    source: Box::new(source),
                }
                .into(),
                kind: SessionRequestFailureKind::Other,
            }
        })?;

        let mut data_request = ctx
            .reqwest_client
            .request(ctx.http.method().clone(), ctx.url)
            .header("Authorization", &voucher_auth)
            .header("Idempotency-Key", &request_idempotency_key);
        data_request = crate::http::HttpClient::apply_body_from(data_request, ctx.http.body());

        let response = data_request
            .send()
            .await
            .map_err(NetworkError::Reqwest)
            .map_err(|source| SessionRequestFailure {
                source: source.into(),
                kind: SessionRequestFailureKind::Other,
            })?;

        let status = response.status();
        if status.as_u16() == 402 || status.is_client_error() || status.is_server_error() {
            let body = response.text().await.unwrap_or_default();

            if !attempted_non_stream_top_up && status.as_u16() == 402 {
                if let Some(problem) = parse_problem_details(&body) {
                    let problem_type = problem.classify();
                    if matches!(
                        problem_type,
                        SessionProblemType::InsufficientBalance
                            | SessionProblemType::AmountExceedsDeposit
                    ) {
                        validate_problem_channel_id(&problem, state.channel_id).map_err(
                            |source| SessionRequestFailure {
                                source,
                                kind: SessionRequestFailureKind::Other,
                            },
                        )?;

                        // Use server-provided requiredTopUp if available,
                        // otherwise compute from cumulative vs deposit.
                        let required_top_up =
                            if let Some(value) = problem.required_top_up.as_deref() {
                                parse_positive_problem_amount(value, "session top-up requiredTopUp")
                                    .map_err(|source| SessionRequestFailure {
                                        source,
                                        kind: SessionRequestFailureKind::Other,
                                    })?
                            } else {
                                state.cumulative_amount.saturating_sub(state.deposit).max(1)
                            };

                        let additional_deposit =
                            if let Some(max_cumulative_spend) = state.max_cumulative_spend {
                                max_cumulative_spend.saturating_sub(state.deposit)
                            } else {
                                required_top_up.max(ctx.top_up_deposit)
                            };

                        if additional_deposit == 0 {
                            let required_cumulative = state.deposit.saturating_add(required_top_up);
                            if let Some(max_cumulative_spend) = state.max_cumulative_spend {
                                return Err(SessionRequestFailure {
                                    source: PaymentError::PaymentRejected {
                                        reason: format!(
                                            "Payment max spend exceeded: max={} required={}",
                                            format_token_amount(
                                                max_cumulative_spend,
                                                ctx.network_id
                                            ),
                                            format_token_amount(
                                                required_cumulative,
                                                ctx.network_id
                                            ),
                                        ),
                                        status_code: 402,
                                    }
                                    .into(),
                                    kind: SessionRequestFailureKind::Other,
                                });
                            }

                            return Err(SessionRequestFailure {
                                source: PaymentError::DepositInsufficient {
                                    deposit: format_token_amount(state.deposit, ctx.network_id),
                                    amount: format_token_amount(
                                        required_cumulative,
                                        ctx.network_id,
                                    ),
                                }
                                .into(),
                                kind: SessionRequestFailureKind::Other,
                            });
                        }

                        match run_non_streaming_top_up_recovery(ctx, state, additional_deposit)
                            .await
                        {
                            Ok(()) => {
                                attempted_non_stream_top_up = true;
                                continue;
                            }
                            Err(source) => {
                                // Top-up tx failed (e.g. channel closed on-chain).
                                // Signal invalidation so the caller can re-open.
                                return Err(SessionRequestFailure {
                                    source,
                                    kind: SessionRequestFailureKind::ChannelInvalidated,
                                });
                            }
                        }
                    }
                }
            }

            let failure_kind = classify_session_failure(status.as_u16(), &body);
            return Err(SessionRequestFailure {
                source: payment_rejected_from_body(status.as_u16(), &body),
                kind: failure_kind,
            });
        }

        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));

        let channel_id = format!("{:#x}", state.channel_id);

        if is_sse {
            streaming::stream_sse_response(ctx, state, response)
                .await
                .map_err(|source| SessionRequestFailure {
                    source,
                    kind: SessionRequestFailureKind::Other,
                })?;
            return Ok(PaymentResult {
                tx_hash: None,
                channel_id: Some(channel_id),
                status_code: 200,
                response: None,
            });
        }

        let http_response = HttpResponse::from_reqwest(response)
            .await
            .map_err(|source| SessionRequestFailure {
                source,
                kind: SessionRequestFailureKind::Other,
            })?;
        let status_code = http_response.status_code;
        let tx_hash = apply_response_receipt(&http_response, state, "session response").map_err(
            |source| SessionRequestFailure {
                source,
                kind: SessionRequestFailureKind::Other,
            },
        )?;

        return Ok(PaymentResult {
            tx_hash,
            channel_id: Some(channel_id),
            status_code,
            response: Some(http_response),
        });
    }
}

/// Handle the full MPP session flow (402 with intent="session").
///
/// This manages the session lifecycle with persistence:
/// 1. Parse the session challenge from the initial 402 response
/// 2. Check for an existing persisted session for this origin
/// 3. If found and not expired, reuse it (skip channel open)
/// 4. If not found or expired, open a new channel on-chain
/// 5. Send the real request with a voucher
/// 6. Stream SSE events (or return buffered response)
/// 7. Persist/update the session (do NOT close the channel)
struct ResolvedSessionChallenge {
    network_id: tempo_common::network::NetworkId,
    chain_id: u64,
    amount: u128,
    suggested_deposit: Option<u128>,
    escrow_contract: Address,
    token: Address,
    payee: Address,
    fee_payer: bool,
    suggested_channel_id: Option<B256>,
    echo: mpp::ChallengeEcho,
    origin: String,
    session_key: String,
    from: Address,
    key_address: Address,
    did: String,
    payer_hex: String,
    payee_hex: String,
    token_hex: String,
}

enum ChallengeStageOutcome {
    DryRun(PaymentResult),
    Continue(Box<ResolvedSessionChallenge>),
}

struct DepositStageOutput {
    provider: alloy::providers::RootProvider<alloy::network::Ethereum>,
    deposit: u128,
    clamped_deposit: Option<u128>,
}

struct ChannelReuseCandidate {
    record: session::ChannelRecord,
    on_chain_deposit: u128,
}

enum ReuseStageOutcome {
    Reused(PaidRequestResult),
    NeedsOpen,
}

/// The open response is either a buffered body (non-SSE) or a raw streaming
/// response (SSE) that must be consumed incrementally.
enum OpenResponse {
    Buffered(HttpResponse),
    Streaming {
        response: reqwest::Response,
        status_code: u16,
    },
}

struct OpenExecutionPlan {
    state: ChannelState,
    salt_hex: String,
    open_tx_hash: Option<String>,
    open_response: OpenResponse,
}

struct PaidRequestResult {
    payment: PaymentResult,
}

fn build_channel_context<'a>(
    signer: &'a Signer,
    http: &'a crate::http::HttpClient,
    url: &'a str,
    rpc_url: &'a str,
    challenge: &'a ResolvedSessionChallenge,
    deposit: &'a DepositStageOutput,
    salt: String,
) -> ChannelContext<'a> {
    ChannelContext {
        signer,
        payer: challenge.from,
        echo: &challenge.echo,
        did: &challenge.did,
        http,
        url,
        rpc_url,
        network_id: challenge.network_id,
        origin: &challenge.origin,
        top_up_deposit: deposit.deposit,
        clamped_deposit: deposit.clamped_deposit,
        fee_payer: challenge.fee_payer,
        salt,
        payee: challenge.payee,
        token: challenge.token,
        reqwest_client: http.client(),
    }
}

fn build_ondemand_reuse_record(
    challenge: &ResolvedSessionChallenge,
    url: &str,
    channel_id: B256,
    on_chain: &session::OnChainChannel,
) -> Result<session::ChannelRecord, TempoError> {
    let now = session::now_secs();
    let challenge_echo = serde_json::to_string(&challenge.echo)
        .map_err(|err| session_store_error("serialize challenge echo", err))?;
    Ok(session::ChannelRecord {
        version: 1,
        origin: challenge.origin.clone(),
        request_url: url.to_string(),
        chain_id: challenge.chain_id,
        escrow_contract: challenge.escrow_contract,
        token: challenge.token_hex.clone(),
        payee: challenge.payee_hex.clone(),
        payer: challenge.payer_hex.clone(),
        authorized_signer: challenge.key_address,
        salt: "0x00".to_string(),
        channel_id,
        deposit: on_chain.deposit,
        cumulative_amount: on_chain.settled,
        accepted_cumulative: on_chain.settled,
        server_spent: 0,
        challenge_echo,
        state: session::ChannelStatus::Active,
        close_requested_at: 0,
        grace_ready_at: 0,
        created_at: now,
        last_used_at: now,
    })
}

async fn challenge_stage(
    http: &crate::http::HttpClient,
    url: &str,
    resolved: &ResolvedChallenge,
    signer: &Signer,
) -> Result<ChallengeStageOutcome, TempoError> {
    let challenge = &resolved.challenge;
    let network_id = resolved.network_id;
    let network_name = network_id.as_str();

    challenge
        .validate_for_session("tempo")
        .map_err(|error| map_mpp_validation_error(error, challenge))?;

    let session_req = decode_session_request(challenge)?;
    let chain_id = require_session_chain_id(&session_req, "session request methodDetails")?;
    let amount = challenge_u128_parse(&session_req.amount, "session request amount")?;
    let suggested_deposit = session_req
        .suggested_deposit
        .as_deref()
        .map(|value| challenge_u128_parse(value, "session request suggestedDeposit"))
        .transpose()?;

    let escrow_str = session_req.escrow_contract().map_err(|_| {
        challenge_missing_field("session challenge escrow contract", "escrow contract")
    })?;
    let escrow_contract: Address = escrow_str
        .parse()
        .map_err(|source| challenge_address_parse("session challenge escrow contract", source))?;
    let expected_escrow = resolved.network_id.escrow_contract();
    if escrow_contract != expected_escrow {
        return Err(PaymentError::ChallengeUntrustedEscrow {
            context: "session challenge escrow contract",
            provided: escrow_str,
            expected: expected_escrow.to_string(),
            network: network_name.to_string(),
        }
        .into());
    }

    let token: Address = session_req
        .currency
        .parse()
        .map_err(|source| challenge_address_parse("session challenge currency", source))?;
    let payee: Address = session_req
        .recipient
        .as_deref()
        .ok_or_else(|| challenge_missing_field("session challenge recipient", "recipient"))?
        .parse()
        .map_err(|source| challenge_address_parse("session challenge recipient", source))?;

    if http.log_enabled() {
        let cost_display = format_token_amount(amount, network_id);
        eprintln!(
            "Cost per {}: {}",
            session_req.unit_type.as_deref().unwrap_or("request"),
            cost_display
        );
    }

    if http.dry_run {
        let cost_display = format_token_amount(amount, network_id);
        println!("[DRY RUN] Session payment would be made:");
        println!("Protocol: MPP (https://mpp.dev)");
        println!("Method: {}", challenge.method);
        println!("Intent: session");
        println!("Network: {network_name}");
        println!(
            "Cost per {}: {}",
            session_req.unit_type.as_deref().unwrap_or("request"),
            cost_display
        );
        println!(
            "Currency: {}",
            address_link(resolved.network_id, &session_req.currency)
        );
        if let Some(ref payee_str) = session_req.recipient {
            println!(
                "Recipient: {}",
                address_link(resolved.network_id, payee_str)
            );
        }
        if let Some(ref suggested) = session_req.suggested_deposit {
            let deposit_val = challenge_u128_parse(suggested, "session request suggestedDeposit")?;
            let deposit_display = format_token_amount(deposit_val, network_id);
            println!("Suggested deposit: {deposit_display}");
        }

        return Ok(ChallengeStageOutcome::DryRun(PaymentResult {
            tx_hash: None,
            channel_id: None,
            status_code: 200,
            response: None,
        }));
    }

    let from = signer.from;
    let key_address = signer.signer.address();
    let fee_payer = session_req.fee_payer();
    let suggested_channel_id = session_req
        .channel_id()
        .as_deref()
        .map(|value| challenge_channel_id_parse(value, "session request methodDetails.channelId"))
        .transpose()?;
    let echo = challenge.to_echo();
    let origin = extract_origin(url);
    let session_key = origin_lock_key(url);
    let payer_hex = format!("{from:#x}");
    let payee_hex = format!("{payee:#x}");
    let token_hex = format!("{token:#x}");
    let did = format!("did:pkh:eip155:{chain_id}:{from:#x}");

    Ok(ChallengeStageOutcome::Continue(Box::new(
        ResolvedSessionChallenge {
            network_id,
            chain_id,
            amount,
            suggested_deposit,
            escrow_contract,
            token,
            payee,
            fee_payer,
            suggested_channel_id,
            echo,
            origin,
            session_key,
            from,
            key_address,
            did,
            payer_hex,
            payee_hex,
            token_hex,
        },
    )))
}

async fn reuse_persisted_channel_without_on_chain_check(
    http: &crate::http::HttpClient,
    url: &str,
    resolved: &ResolvedChallenge,
    signer: &Signer,
    challenge: &ResolvedSessionChallenge,
    record: session::ChannelRecord,
    max_spend: Option<u128>,
) -> Result<Option<PaymentResult>, TempoError> {
    if !is_session_reusable(
        &challenge.origin,
        &record,
        &challenge.payer_hex,
        challenge.escrow_contract,
        &challenge.token_hex,
        &challenge.payee_hex,
        challenge.chain_id,
        challenge.key_address,
    ) {
        return Ok(None);
    }

    if let Some(max_spend) = max_spend {
        if challenge.amount > max_spend {
            return Err(PaymentError::PaymentRejected {
                reason: format!(
                    "Payment max spend exceeded: max={} required={}",
                    format_token_amount(max_spend, challenge.network_id),
                    format_token_amount(challenge.amount, challenge.network_id),
                ),
                status_code: 402,
            }
            .into());
        }
    }

    let top_up_deposit = challenge
        .suggested_deposit
        .unwrap_or(record.deposit)
        .max(challenge.amount);
    let deposit = DepositStageOutput {
        provider: alloy::providers::RootProvider::new_http(resolved.rpc_url.clone()),
        deposit: top_up_deposit,
        clamped_deposit: None,
    };
    let reusable = ChannelReuseCandidate {
        on_chain_deposit: record.deposit,
        record,
    };

    match reuse_stage_execute(
        http, url, resolved, signer, challenge, &deposit, reusable, max_spend,
    )
    .await?
    {
        ReuseStageOutcome::Reused(result) => Ok(Some(result.payment)),
        ReuseStageOutcome::NeedsOpen => Ok(None),
    }
}

async fn deposit_stage(
    resolved: &ResolvedChallenge,
    challenge: &ResolvedSessionChallenge,
    max_spend: Option<u128>,
) -> Result<DepositStageOutput, TempoError> {
    let decimals = challenge.network_id.token().decimals;
    let default_deposit: u128 = parse_units("1", decimals).unwrap().get_absolute().to();
    let max_deposit: u128 = parse_units("5", decimals).unwrap().get_absolute().to();

    let mut deposit = if let Some(max_spend) = max_spend {
        max_spend.max(challenge.amount)
    } else {
        challenge
            .suggested_deposit
            .unwrap_or(default_deposit)
            .max(challenge.amount)
            .min(max_deposit)
    };
    let mut clamped_deposit = None;

    let provider = alloy::providers::RootProvider::new_http(resolved.rpc_url.clone());
    if let Ok(balance) =
        session::query_token_balance(&provider, challenge.token, challenge.from).await
    {
        let balance_u128: u128 = balance.try_into().unwrap_or(u128::MAX);
        let usable = balance_u128 / 2;
        if usable < deposit {
            deposit = usable;
            clamped_deposit = Some(deposit);
        }
    }

    Ok(DepositStageOutput {
        provider,
        deposit,
        clamped_deposit,
    })
}

async fn reuse_stage_discover(
    url: &str,
    challenge: &ResolvedSessionChallenge,
    deposit: &DepositStageOutput,
) -> Result<Option<ChannelReuseCandidate>, TempoError> {
    let mut reuse_candidates: Vec<session::ChannelRecord> = Vec::new();

    if let Some(channel_id) = challenge.suggested_channel_id {
        let channel_id_hex = format!("{channel_id:#x}");
        let loaded = session::load_channel(&channel_id_hex)
            .map_err(|err| session_store_error("load channel", err))?;

        if let Some(record) = loaded {
            reuse_candidates.push(record);
        } else if let Some(on_chain_availability) = validate_reusable_channel_on_chain(
            &deposit.provider,
            challenge.escrow_contract,
            channel_id,
            challenge.amount,
        )
        .await?
        {
            if is_on_chain_identity_match(
                &on_chain_availability.on_chain,
                challenge.from,
                challenge.payee,
                challenge.token,
                challenge.key_address,
            ) {
                reuse_candidates.push(build_ondemand_reuse_record(
                    challenge,
                    url,
                    channel_id,
                    &on_chain_availability.on_chain,
                )?);
            }
        }
    }

    if let Some(record) = session::find_reusable_channel(
        &challenge.origin,
        &challenge.payer_hex,
        challenge.escrow_contract,
        &challenge.token_hex,
        &challenge.payee_hex,
        challenge.chain_id,
    )
    .map_err(|err| session_store_error("load session", err))?
    {
        let seen = reuse_candidates.iter().any(|candidate| {
            normalize_hex_identifier(&candidate.channel_id_hex())
                == normalize_hex_identifier(&record.channel_id_hex())
        });
        if !seen {
            reuse_candidates.push(record);
        }
    }

    for candidate in reuse_candidates {
        if !is_session_reusable(
            &challenge.origin,
            &candidate,
            &challenge.payer_hex,
            challenge.escrow_contract,
            &challenge.token_hex,
            &challenge.payee_hex,
            challenge.chain_id,
            challenge.key_address,
        ) {
            continue;
        }

        let Some(on_chain_availability) = validate_reusable_channel_on_chain(
            &deposit.provider,
            challenge.escrow_contract,
            candidate.channel_id,
            challenge.amount,
        )
        .await?
        else {
            continue;
        };

        if !is_on_chain_identity_match(
            &on_chain_availability.on_chain,
            challenge.from,
            challenge.payee,
            challenge.token,
            challenge.key_address,
        ) {
            continue;
        }

        return Ok(Some(ChannelReuseCandidate {
            record: candidate,
            on_chain_deposit: on_chain_availability.on_chain.deposit,
        }));
    }

    Ok(None)
}

#[allow(clippy::too_many_arguments)]
async fn reuse_stage_execute(
    http: &crate::http::HttpClient,
    url: &str,
    resolved: &ResolvedChallenge,
    signer: &Signer,
    challenge: &ResolvedSessionChallenge,
    deposit: &DepositStageOutput,
    reusable: ChannelReuseCandidate,
    max_spend: Option<u128>,
) -> Result<ReuseStageOutcome, TempoError> {
    if http.log_enabled() {
        eprintln!(
            "Reusing session {} for {}",
            reusable.record.channel_id_hex(),
            challenge.origin
        );
    }

    let channel_id = reusable.record.channel_id;
    let prev_cumulative = reusable.record.cumulative_amount_u128();
    let mut state = ChannelState {
        channel_id,
        escrow_contract: challenge.escrow_contract,
        chain_id: challenge.chain_id,
        deposit: reusable.on_chain_deposit,
        cumulative_amount: prev_cumulative + challenge.amount,
        accepted_cumulative: reusable.record.accepted_cumulative,
        max_cumulative_spend: max_spend,
        server_spent: reusable.record.server_spent,
    };

    validate_request_spend_limit(&state, challenge.network_id, state.cumulative_amount)?;

    let ctx = build_channel_context(
        signer,
        http,
        url,
        resolved.rpc_url.as_str(),
        challenge,
        deposit,
        reusable.record.salt.clone(),
    );

    match send_session_request(&ctx, &mut state).await {
        Ok(result) => {
            persist_session(&ctx, &state)?;
            Ok(ReuseStageOutcome::Reused(PaidRequestResult {
                payment: result,
            }))
        }
        Err(failure) => {
            if failure.kind == SessionRequestFailureKind::ChannelInvalidated {
                let invalidation_confirmed = match confirm_server_invalidation_on_chain(
                    &deposit.provider,
                    challenge.escrow_contract,
                    channel_id,
                )
                .await
                {
                    Ok(confirmed) => confirmed,
                    Err(source) => {
                        persist_session(&ctx, &state)
                            .map_err(session_reuse_persist_failed_error)?;
                        return Err(session_reuse_preserved_error(source));
                    }
                };

                if invalidation_confirmed {
                    let channel_id_hex = format!("{channel_id:#x}");
                    session::delete_channel(&channel_id_hex)
                        .map_err(session_reuse_cleanup_failed_error)?;
                    if http.log_enabled() {
                        eprintln!(
                            "Persisted channel {channel_id_hex} rejected by server and confirmed on-chain; opening a new channel"
                        );
                    }
                    Ok(ReuseStageOutcome::NeedsOpen)
                } else {
                    persist_session(&ctx, &state).map_err(session_reuse_persist_failed_error)?;
                    Err(session_reuse_preserved_error(
                        unconfirmed_invalidation_error(channel_id),
                    ))
                }
            } else {
                persist_session(&ctx, &state).map_err(session_reuse_persist_failed_error)?;
                Err(session_reuse_preserved_error(failure.into_tempo()))
            }
        }
    }
}

/// Build the open tx, format the credential, and send to the server.
///
/// Extracted so `open_stage` can retry with a provisioning signer when the
/// server rejects the optimistic (no key_authorization) transaction.
#[allow(clippy::too_many_arguments)]
async fn build_and_send_open(
    http: &crate::http::HttpClient,
    url: &str,
    resolved: &ResolvedChallenge,
    signer: &Signer,
    challenge: &ResolvedSessionChallenge,
    open_calls: Vec<tempo_primitives::transaction::Call>,
    channel_id: B256,
    initial_cumulative: u128,
    voucher_sig: &[u8],
    idempotency_key: &str,
    delays: &[u64],
) -> Result<reqwest::Response, TempoError> {
    let payment = open::create_tempo_payment_from_calls(
        resolved.rpc_url.as_str(),
        signer,
        open_calls,
        challenge.token,
        challenge.chain_id,
        challenge.fee_payer,
    )
    .await?;

    let open_tx = format!("0x{}", hex::encode(&payment.tx_bytes));
    let open_payload = build_open_payload(
        channel_id,
        open_tx,
        challenge.key_address,
        initial_cumulative,
        voucher_sig,
    );

    let session_credential = mpp::PaymentCredential::with_source(
        challenge.echo.clone(),
        challenge.did.clone(),
        open_payload,
    );
    let auth_header = mpp::format_authorization(&session_credential).map_err(|source| {
        PaymentError::ChallengeFormatSource {
            context: "open credential",
            source: Box::new(source),
        }
    })?;

    open::send_open_with_retry(http, url, &auth_header, idempotency_key, delays).await
}

async fn open_stage(
    http: &crate::http::HttpClient,
    url: &str,
    resolved: &ResolvedChallenge,
    signer: &Signer,
    challenge: &ResolvedSessionChallenge,
    deposit: &DepositStageOutput,
    max_spend: Option<u128>,
) -> Result<OpenExecutionPlan, TempoError> {
    let salt = B256::random();
    let channel_id = compute_channel_id(
        challenge.from,
        challenge.payee,
        challenge.token,
        salt,
        challenge.key_address,
        challenge.escrow_contract,
        challenge.chain_id,
    );

    if http.log_enabled() {
        if let Some(clamped) = deposit.clamped_deposit {
            eprintln!(
                "Clamping deposit to 50% of wallet balance: {}",
                format_token_amount(clamped, challenge.network_id)
            );
        }
        let deposit_display = format_token_amount(deposit.deposit, challenge.network_id);
        eprintln!("Opening channel {channel_id:#x} (deposit: {deposit_display})");
    }

    let open_calls = session::build_open_calls(
        challenge.token,
        challenge.escrow_contract,
        deposit.deposit,
        challenge.payee,
        salt,
        challenge.key_address,
    );

    let initial_cumulative = challenge.amount;
    let voucher_sig = sign_voucher(
        &signer.signer,
        channel_id,
        initial_cumulative,
        challenge.escrow_contract,
        challenge.chain_id,
    )
    .await
    .map_err(|source| KeyError::SigningOperationSource {
        operation: "sign initial voucher",
        source: Box::new(source),
    })?;

    let open_idempotency_key = new_idempotency_key();
    let delays = [2000_u64, 3000, 5000];

    let mut state = ChannelState {
        channel_id,
        escrow_contract: challenge.escrow_contract,
        chain_id: challenge.chain_id,
        deposit: deposit.deposit,
        cumulative_amount: initial_cumulative,
        accepted_cumulative: 0,
        max_cumulative_spend: max_spend,
        server_spent: 0,
    };

    validate_request_spend_limit(&state, challenge.network_id, state.cumulative_amount)?;
    let salt_hex = format!("{salt:#x}");

    // Helper: persist channel state on error so on-chain funds aren't orphaned.
    // The tx may have been broadcast by the server even if the response was an error.
    let persist_on_error = |state: &ChannelState, e: TempoError| -> TempoError {
        let ctx = build_channel_context(
            signer,
            http,
            url,
            resolved.rpc_url.as_str(),
            challenge,
            deposit,
            salt_hex.clone(),
        );
        let _ = persist_session(&ctx, state);
        e
    };

    let open_response = match build_and_send_open(
        http,
        url,
        resolved,
        signer,
        challenge,
        open_calls.clone(),
        channel_id,
        initial_cumulative,
        &voucher_sig,
        &open_idempotency_key,
        &delays,
    )
    .await
    {
        Ok(resp) => resp,
        Err(original) if signer.has_stored_key_authorization() => {
            if http.log_enabled() {
                eprintln!("Key not provisioned on-chain, retrying with authorization...");
            }
            let provisioning_signer =
                signer
                    .with_key_authorization()
                    .ok_or_else(|| KeyError::SigningOperation {
                        operation: "key provisioning",
                        reason: "stored key authorization could not be applied to signing mode"
                            .to_string(),
                    })?;
            let retry_idempotency_key = new_idempotency_key();
            build_and_send_open(
                http,
                url,
                resolved,
                &provisioning_signer,
                challenge,
                open_calls,
                channel_id,
                initial_cumulative,
                &voucher_sig,
                &retry_idempotency_key,
                &delays,
            )
            .await
            .map_err(|_| persist_on_error(&state, original))?
        }
        Err(e) => return Err(persist_on_error(&state, e)),
    };

    // Detect SSE before consuming the body. For SSE streams we must pass
    // the raw response through so `stream_sse_response` can read chunks
    // incrementally. Non-SSE responses are buffered as before.
    let is_sse = open_response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));

    if is_sse {
        let status_code = open_response.status().as_u16();
        // Compatibility: some providers send the required receipt only as an
        // SSE `payment-receipt` event rather than an initial HTTP header.
        let open_tx_hash =
            apply_response_receipt_from_headers(&open_response, &mut state, "open response", false)
                .map_err(|e| persist_on_error(&state, e))?;
        if let Some(tx_ref) = open_tx_hash.as_deref() {
            if http.log_enabled() {
                eprintln!("Channel open tx: {}", challenge.network_id.tx_url(tx_ref));
            }
        }
        return Ok(OpenExecutionPlan {
            state,
            salt_hex,
            open_tx_hash,
            open_response: OpenResponse::Streaming {
                response: open_response,
                status_code,
            },
        });
    }

    let buffered = HttpResponse::from_reqwest(open_response).await?;
    let open_tx_hash = apply_response_receipt(&buffered, &mut state, "open response")
        .map_err(|e| persist_on_error(&state, e))?;
    if let Some(tx_ref) = open_tx_hash.as_deref() {
        if http.log_enabled() {
            eprintln!("Channel open tx: {}", challenge.network_id.tx_url(tx_ref));
        }
    }

    Ok(OpenExecutionPlan {
        state,
        salt_hex,
        open_tx_hash,
        open_response: OpenResponse::Buffered(buffered),
    })
}

async fn request_stage(
    ctx: &ChannelContext<'_>,
    state: &mut ChannelState,
) -> Result<PaidRequestResult, TempoError> {
    persist_session(ctx, state)?;
    let result = send_session_request(ctx, state)
        .await
        .map_err(SessionRequestFailure::into_tempo)?;
    persist_session(ctx, state)?;
    Ok(PaidRequestResult { payment: result })
}

pub(crate) async fn handle_session_request(
    http: &crate::http::HttpClient,
    url: &str,
    resolved: ResolvedChallenge,
    signer: Signer,
    _keys: &Keystore,
) -> Result<PaymentResult, TempoError> {
    let challenge = match challenge_stage(http, url, &resolved, &signer).await? {
        ChallengeStageOutcome::DryRun(result) => return Ok(result),
        ChallengeStageOutcome::Continue(data) => *data,
    };

    let max_spend = parse_max_spend(http.max_spend.as_deref(), challenge.network_id)?;
    if let Some(max_spend) = max_spend {
        if challenge.amount > max_spend {
            return Err(PaymentError::PaymentRejected {
                reason: format!(
                    "Payment max spend exceeded: max={} required={}",
                    format_token_amount(max_spend, challenge.network_id),
                    format_token_amount(challenge.amount, challenge.network_id),
                ),
                status_code: 402,
            }
            .into());
        }
    }

    // Hold a blocking per-origin lock for the full paid request lifecycle.
    // A channel permits only one active session at a time, so requests to
    // the same origin are intentionally serialized until completion.
    let _lock_guard = acquire_origin_lock(&challenge.session_key)
        .map_err(|e| session_store_error("acquire session lock", e))?;

    if let Some(record) = session::find_reusable_channel(
        &challenge.origin,
        &challenge.payer_hex,
        challenge.escrow_contract,
        &challenge.token_hex,
        &challenge.payee_hex,
        challenge.chain_id,
    )
    .map_err(|err| session_store_error("load session", err))?
    {
        if let Some(result) = reuse_persisted_channel_without_on_chain_check(
            http, url, &resolved, &signer, &challenge, record, max_spend,
        )
        .await?
        {
            return Ok(result);
        }
    }

    let deposit = deposit_stage(&resolved, &challenge, max_spend).await?;

    if let Some(reusable) = reuse_stage_discover(url, &challenge, &deposit).await? {
        match reuse_stage_execute(
            http, url, &resolved, &signer, &challenge, &deposit, reusable, max_spend,
        )
        .await?
        {
            ReuseStageOutcome::Reused(result) => return Ok(result.payment),
            ReuseStageOutcome::NeedsOpen => {}
        }
    }

    if deposit.deposit < challenge.amount {
        return Err(PaymentError::DepositInsufficient {
            deposit: format_token_amount(deposit.deposit, challenge.network_id),
            amount: format_token_amount(challenge.amount, challenge.network_id),
        }
        .into());
    }

    let mut opened = open_stage(
        http, url, &resolved, &signer, &challenge, &deposit, max_spend,
    )
    .await?;
    let ctx = build_channel_context(
        &signer,
        http,
        url,
        resolved.rpc_url.as_str(),
        &challenge,
        &deposit,
        opened.salt_hex.clone(),
    );

    match opened.open_response {
        // SSE stream: pipe the raw response directly to the streaming handler
        // so tokens are printed incrementally instead of buffering the entire body.
        OpenResponse::Streaming {
            response: raw_response,
            status_code,
        } => {
            persist_session(&ctx, &opened.state)?;
            streaming::stream_sse_response(&ctx, &mut opened.state, raw_response).await?;
            persist_session(&ctx, &opened.state)?;
            Ok(PaymentResult {
                tx_hash: opened.open_tx_hash,
                channel_id: Some(format!("{:#x}", opened.state.channel_id)),
                status_code,
                response: None,
            })
        }
        // Non-SSE: the open response already contains the proxied upstream
        // result — use it directly instead of making a duplicate request.
        OpenResponse::Buffered(buffered) if buffered.status_code < 400 => {
            persist_session(&ctx, &opened.state)?;
            Ok(PaymentResult {
                tx_hash: opened.open_tx_hash,
                channel_id: Some(format!("{:#x}", opened.state.channel_id)),
                status_code: buffered.status_code,
                response: Some(buffered),
            })
        }
        // Buffered error response: fall through to a separate paid request.
        OpenResponse::Buffered(_) => request_stage(&ctx, &mut opened.state)
            .await
            .map(|result| result.payment),
    }
}

/// Check whether a persisted session record can be reused for a new request.
///
/// Reuse requires matching payer and channel identity fields (escrow,
/// token, payee, chain). The request price is not checked —
/// it is pricing metadata, not channel identity, and varying prices
/// (e.g. different models on the same origin) must not cause channel churn.
#[allow(clippy::too_many_arguments)]
fn is_session_reusable(
    challenge_origin: &str,
    record: &tempo_common::session::ChannelRecord,
    payer: &str,
    escrow: Address,
    token: &str,
    payee: &str,
    chain_id: u64,
    authorized_signer: Address,
) -> bool {
    record.origin == challenge_origin
        && record.state == tempo_common::session::ChannelStatus::Active
        && normalize_hex_identifier(&record.payer) == normalize_hex_identifier(payer)
        && record.escrow_contract == escrow
        && normalize_hex_identifier(&record.token) == normalize_hex_identifier(token)
        && normalize_hex_identifier(&record.payee) == normalize_hex_identifier(payee)
        && record.chain_id == chain_id
        && record.authorized_signer == authorized_signer
}

#[cfg(test)]
mod tests {
    use super::{
        apply_response_receipt, apply_response_receipt_from_headers, assess_on_chain_reusability,
        challenge_channel_id_parse, classify_session_failure, invalidation_confirmed_on_chain,
        is_on_chain_identity_match, is_session_reusable, normalize_hex_identifier,
        parse_positive_problem_amount, session_reuse_persist_failed_error, session_store_error,
        validate_problem_channel_id, SessionRequestFailureKind,
    };
    use crate::http::HttpResponse;
    use alloy::primitives::{Address, B256};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use mpp::protocol::methods::tempo::SessionReceipt;
    use tempo_common::{
        error::{PaymentError, TempoError},
        payment::{
            classify::ProblemDetails,
            session::{now_secs, ChannelRecord, ChannelStatus, OnChainChannel},
        },
    };

    fn make_record() -> ChannelRecord {
        let now = now_secs();
        ChannelRecord {
            version: 1,
            origin: "https://openrouter.mpp.tempo.xyz".into(),
            request_url: "https://openrouter.mpp.tempo.xyz/v1/chat/completions".into(),
            chain_id: 4217,
            escrow_contract: Address::ZERO,
            token: "0x0000000000000000000000000000000000000001".into(),
            payee: "0x0000000000000000000000000000000000000002".into(),
            payer: "0x0000000000000000000000000000000000000099".into(),
            authorized_signer: Address::ZERO,
            salt: "0x00".into(),
            channel_id: alloy::primitives::B256::ZERO,
            deposit: 1_000_000,
            cumulative_amount: 500,
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

    fn encode_receipt_header(receipt: &SessionReceipt) -> String {
        let json = serde_json::to_vec(receipt).unwrap();
        URL_SAFE_NO_PAD.encode(json)
    }

    #[test]
    fn session_store_error_maps_to_payment_error() {
        let err = session_store_error("load session", std::io::Error::other("sqlite busy"));
        match err {
            TempoError::Payment(PaymentError::ChannelPersistenceSource { operation, source }) => {
                assert_eq!(operation, "load session");
                assert_eq!(source.to_string(), "sqlite busy");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn session_reuse_persist_failed_error_wraps_with_context() {
        let source = TempoError::Payment(PaymentError::PaymentRejected {
            reason: "boom".to_string(),
            status_code: 500,
        });
        let err = session_reuse_persist_failed_error(source);
        let msg = err.to_string();
        assert!(msg.contains("Failed to persist preserved channel state for on-chain dispute"));
        assert!(msg.contains("Payment rejected by server: boom"));
    }

    #[test]
    fn reuse_same_channel_identity() {
        let record = make_record();
        assert!(is_session_reusable(
            "https://openrouter.mpp.tempo.xyz",
            &record,
            "0x0000000000000000000000000000000000000099",
            Address::ZERO,
            "0x0000000000000000000000000000000000000001",
            "0x0000000000000000000000000000000000000002",
            4217,
            Address::ZERO,
        ));
    }

    #[test]
    fn reuse_rejects_different_payer() {
        let record = make_record();
        assert!(!is_session_reusable(
            "https://openrouter.mpp.tempo.xyz",
            &record,
            "0xdifferentpayer",
            Address::ZERO,
            "0x0000000000000000000000000000000000000001",
            "0x0000000000000000000000000000000000000002",
            4217,
            Address::ZERO,
        ));
    }

    #[test]
    fn reuse_rejects_different_recipient() {
        let record = make_record();
        assert!(!is_session_reusable(
            "https://openrouter.mpp.tempo.xyz",
            &record,
            "0x0000000000000000000000000000000000000099",
            Address::ZERO,
            "0x0000000000000000000000000000000000000001",
            "0xdifferentrecipient",
            4217,
            Address::ZERO,
        ));
    }

    #[test]
    fn reuse_rejects_different_chain() {
        let record = make_record();
        assert!(!is_session_reusable(
            "https://openrouter.mpp.tempo.xyz",
            &record,
            "0x0000000000000000000000000000000000000099",
            Address::ZERO,
            "0x0000000000000000000000000000000000000001",
            "0x0000000000000000000000000000000000000002",
            42431, // different chain
            Address::ZERO,
        ));
    }

    #[test]
    fn reuse_rejects_different_authorized_signer() {
        let record = make_record();
        assert!(!is_session_reusable(
            "https://openrouter.mpp.tempo.xyz",
            &record,
            "0x0000000000000000000000000000000000000099",
            Address::ZERO,
            "0x0000000000000000000000000000000000000001",
            "0x0000000000000000000000000000000000000002",
            4217,
            Address::from([0x01; 20]),
        ));
    }

    #[test]
    fn reuse_normalizes_hex_fields() {
        let mut record = make_record();
        record.token = "0X0000000000000000000000000000000000000001".into();
        record.payee = "0X0000000000000000000000000000000000000002".into();
        record.payer = "0X0000000000000000000000000000000000000099".into();
        assert!(is_session_reusable(
            "https://openrouter.mpp.tempo.xyz",
            &record,
            "0x0000000000000000000000000000000000000099",
            Address::ZERO,
            "0x0000000000000000000000000000000000000001",
            "0x0000000000000000000000000000000000000002",
            4217,
            Address::ZERO,
        ));
    }

    #[test]
    fn channel_not_found_problem_is_recoverable() {
        let body = r#"{"type":"https://paymentauth.org/problems/session/channel-not-found","detail":"not found"}"#;
        assert_eq!(
            classify_session_failure(410, body),
            SessionRequestFailureKind::ChannelInvalidated
        );
    }

    #[test]
    fn unknown_problem_is_not_recoverable() {
        let body = r#"{"type":"https://paymentauth.org/problems/session/invalid-signature","detail":"bad sig"}"#;
        assert_eq!(
            classify_session_failure(402, body),
            SessionRequestFailureKind::Other
        );
    }

    #[test]
    fn invalidation_confirmed_on_chain_when_missing() {
        assert!(invalidation_confirmed_on_chain(None));
    }

    #[test]
    fn invalidation_confirmed_on_chain_when_close_requested() {
        let channel = OnChainChannel {
            payer: Address::ZERO,
            payee: Address::ZERO,
            token: Address::ZERO,
            authorized_signer: Address::ZERO,
            deposit: 1,
            settled: 0,
            close_requested_at: 123,
        };

        assert!(invalidation_confirmed_on_chain(Some(&channel)));
    }

    #[test]
    fn invalidation_not_confirmed_on_chain_for_active_channel() {
        let channel = OnChainChannel {
            payer: Address::ZERO,
            payee: Address::ZERO,
            token: Address::ZERO,
            authorized_signer: Address::ZERO,
            deposit: 1,
            settled: 0,
            close_requested_at: 0,
        };

        assert!(!invalidation_confirmed_on_chain(Some(&channel)));
    }

    #[test]
    fn normalize_hex_identifier_handles_prefix_case() {
        assert_eq!(normalize_hex_identifier("0XABCDEF"), "0xabcdef".to_string());
    }

    #[test]
    fn challenge_channel_id_parse_rejects_invalid_bytes32() {
        assert!(
            challenge_channel_id_parse("0x1234", "ctx").is_err(),
            "short channel IDs must be rejected"
        );
    }

    #[test]
    fn challenge_channel_id_parse_sanitizes_control_chars() {
        let err = challenge_channel_id_parse("0x12\u{1b}[31m", "ctx").unwrap_err();
        let msg = err.to_string();
        assert!(!msg.chars().any(char::is_control));
        assert!(msg.contains("0x12[31m"));
    }

    #[test]
    fn parse_positive_problem_amount_rejects_zero() {
        let err = parse_positive_problem_amount("0", "ctx").unwrap_err();
        assert!(err.to_string().contains("must be > 0"));
    }

    #[test]
    fn validate_problem_channel_id_rejects_mismatch() {
        let expected = B256::from([0x11; 32]);
        let problem = ProblemDetails {
            problem_type: "https://paymentauth.org/problems/session/insufficient-balance"
                .to_string(),
            title: None,
            status: Some(402),
            detail: Some("need more deposit".to_string()),
            required_top_up: Some("100".to_string()),
            channel_id: Some(format!("{:#x}", B256::from([0x22; 32]))),
            extensions: std::collections::BTreeMap::new(),
        };

        let err = validate_problem_channel_id(&problem, expected).unwrap_err();
        assert!(err.to_string().contains("channelId mismatch"));
    }

    #[test]
    fn on_chain_reusability_requires_open_channel_and_balance() {
        let channel = tempo_common::session::OnChainChannel {
            payer: Address::ZERO,
            payee: Address::ZERO,
            token: Address::ZERO,
            authorized_signer: Address::ZERO,
            deposit: 1_000,
            settled: 700,
            close_requested_at: 0,
        };
        assert_eq!(assess_on_chain_reusability(&channel, 200), Some(300));
        assert_eq!(assess_on_chain_reusability(&channel, 400), None);

        let closing_channel = tempo_common::session::OnChainChannel {
            close_requested_at: 1,
            ..channel
        };
        assert_eq!(assess_on_chain_reusability(&closing_channel, 100), None);
    }

    #[test]
    fn on_chain_identity_match_checks_all_identity_fields() {
        let channel = tempo_common::session::OnChainChannel {
            payer: Address::from([0x11; 20]),
            payee: Address::from([0x22; 20]),
            token: Address::from([0x33; 20]),
            authorized_signer: Address::from([0x44; 20]),
            deposit: 1,
            settled: 0,
            close_requested_at: 0,
        };

        assert!(is_on_chain_identity_match(
            &channel,
            Address::from([0x11; 20]),
            Address::from([0x22; 20]),
            Address::from([0x33; 20]),
            Address::from([0x44; 20])
        ));
        assert!(!is_on_chain_identity_match(
            &channel,
            Address::from([0x10; 20]),
            Address::from([0x22; 20]),
            Address::from([0x33; 20]),
            Address::from([0x44; 20])
        ));
    }

    #[test]
    fn reuse_rejects_non_active_state() {
        let mut record = make_record();
        record.state = ChannelStatus::Closing;
        assert!(!is_session_reusable(
            "https://openrouter.mpp.tempo.xyz",
            &record,
            "0x0000000000000000000000000000000000000099",
            Address::ZERO,
            "0x0000000000000000000000000000000000000001",
            "0x0000000000000000000000000000000000000002",
            4217,
            Address::ZERO,
        ));
    }

    #[test]
    fn reuse_rejects_cross_origin_candidate() {
        let record = make_record();
        assert!(!is_session_reusable(
            "https://malicious.example",
            &record,
            "0x0000000000000000000000000000000000000099",
            Address::ZERO,
            "0x0000000000000000000000000000000000000001",
            "0x0000000000000000000000000000000000000002",
            4217,
            Address::ZERO,
        ));
    }

    #[test]
    fn apply_response_receipt_uses_monotonic_cumulative_floor() {
        let channel_id = B256::from([0x66; 32]);
        let receipt = SessionReceipt {
            method: "tempo".to_string(),
            intent: "session".to_string(),
            status: "success".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            reference: format!("{channel_id:#x}"),
            challenge_id: "challenge".to_string(),
            channel_id: format!("{channel_id:#x}"),
            accepted_cumulative: "10".to_string(),
            spent: "10".to_string(),
            units: Some(1),
            tx_hash: None,
        };
        let header = encode_receipt_header(&receipt);
        let response = HttpResponse::for_test_with_headers(
            200,
            b"ok",
            &[("payment-receipt", header.as_str())],
        );

        let mut state = super::ChannelState {
            channel_id,
            escrow_contract: Address::ZERO,
            chain_id: 4217,
            deposit: 100,
            cumulative_amount: 20,
            accepted_cumulative: 0,
            max_cumulative_spend: None,
            server_spent: 0,
        };
        let tx = apply_response_receipt(&response, &mut state, "session response").unwrap();
        assert_eq!(
            state.cumulative_amount, 20,
            "cumulative should not decrease"
        );
        assert_eq!(state.server_spent, 10, "server spent should be captured");
        assert!(tx.is_some(), "tx reference should be extracted");
    }

    #[test]
    fn apply_response_receipt_requires_valid_header() {
        let channel_id = B256::from([0x78; 32]);
        let mut state_missing = super::ChannelState {
            channel_id,
            escrow_contract: Address::ZERO,
            chain_id: 4217,
            deposit: 100,
            cumulative_amount: 20,
            accepted_cumulative: 0,
            max_cumulative_spend: None,
            server_spent: 0,
        };

        let missing = HttpResponse::for_test(200, b"ok");
        let err =
            apply_response_receipt(&missing, &mut state_missing, "session response").unwrap_err();
        assert!(
            err.to_string().contains("Missing required Payment-Receipt"),
            "expected strict mode to reject missing receipt, got: {err}"
        );

        let mut state_invalid = state_missing;
        let invalid =
            HttpResponse::for_test_with_headers(200, b"ok", &[("payment-receipt", "not-base64")]);
        let err =
            apply_response_receipt(&invalid, &mut state_invalid, "session response").unwrap_err();
        assert!(
            err.to_string().contains("Invalid required Payment-Receipt"),
            "expected strict mode to reject malformed receipt, got: {err}"
        );
    }

    #[test]
    fn apply_response_receipt_strict_allows_reconciled_lower_spent() {
        let channel_id = B256::from([0x79; 32]);
        let receipt = SessionReceipt {
            method: "tempo".to_string(),
            intent: "session".to_string(),
            status: "success".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            reference: format!("{channel_id:#x}"),
            challenge_id: "challenge".to_string(),
            channel_id: format!("{channel_id:#x}"),
            accepted_cumulative: "30".to_string(),
            spent: "7".to_string(),
            units: Some(1),
            tx_hash: None,
        };
        let header = encode_receipt_header(&receipt);
        let response = HttpResponse::for_test_with_headers(
            200,
            b"ok",
            &[("payment-receipt", header.as_str())],
        );

        let mut state = super::ChannelState {
            channel_id,
            escrow_contract: Address::ZERO,
            chain_id: 4217,
            deposit: 100,
            cumulative_amount: 20,
            accepted_cumulative: 20,
            max_cumulative_spend: None,
            server_spent: 10,
        };

        let result = apply_response_receipt(&response, &mut state, "session response");
        assert!(
            result.is_ok(),
            "strict receipt should allow downward spend reconciliation"
        );
        assert_eq!(state.server_spent, 7);
        assert_eq!(state.accepted_cumulative, 30);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_response_receipt_from_headers_allows_missing_header_when_optional() {
        let channel_id = B256::from([0x7A; 32]);
        let app = axum::Router::new().route(
            "/missing",
            axum::routing::get(|| async { (axum::http::StatusCode::OK, "ok") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client
            .get(format!("http://{addr}/missing"))
            .send()
            .await
            .unwrap();

        let mut state = super::ChannelState {
            channel_id,
            escrow_contract: Address::ZERO,
            chain_id: 4217,
            deposit: 100,
            cumulative_amount: 20,
            accepted_cumulative: 20,
            max_cumulative_spend: None,
            server_spent: 10,
        };

        let tx_ref =
            apply_response_receipt_from_headers(&response, &mut state, "open response", false)
                .expect("optional mode should allow missing receipt header");
        assert!(tx_ref.is_none());
        assert_eq!(state.accepted_cumulative, 20);
        assert_eq!(state.server_spent, 10);

        server.abort();
        let _ = server.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_response_receipt_from_headers_requires_header_when_strict() {
        let channel_id = B256::from([0x7C; 32]);
        let app = axum::Router::new().route(
            "/missing-strict",
            axum::routing::get(|| async { (axum::http::StatusCode::OK, "ok") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client
            .get(format!("http://{addr}/missing-strict"))
            .send()
            .await
            .unwrap();

        let mut state = super::ChannelState {
            channel_id,
            escrow_contract: Address::ZERO,
            chain_id: 4217,
            deposit: 100,
            cumulative_amount: 20,
            accepted_cumulative: 20,
            max_cumulative_spend: None,
            server_spent: 10,
        };

        let err = apply_response_receipt_from_headers(&response, &mut state, "open response", true)
            .expect_err("strict mode should reject missing receipt header");
        assert!(err.to_string().contains("Missing required Payment-Receipt"));

        server.abort();
        let _ = server.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_response_receipt_from_headers_requires_valid_spent() {
        let channel_id = B256::from([0x7B; 32]);
        let receipt = SessionReceipt {
            method: "tempo".to_string(),
            intent: "session".to_string(),
            status: "success".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            reference: format!("{channel_id:#x}"),
            challenge_id: "challenge".to_string(),
            channel_id: format!("{channel_id:#x}"),
            accepted_cumulative: "30".to_string(),
            spent: "not-a-number".to_string(),
            units: Some(1),
            tx_hash: None,
        };
        let header = encode_receipt_header(&receipt);

        let app = axum::Router::new().route(
            "/invalid",
            axum::routing::get(move || {
                let header = header.clone();
                async move {
                    axum::http::Response::builder()
                        .status(axum::http::StatusCode::OK)
                        .header("payment-receipt", header)
                        .body(axum::body::Body::from("ok"))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client
            .get(format!("http://{addr}/invalid"))
            .send()
            .await
            .unwrap();

        let mut state = super::ChannelState {
            channel_id,
            escrow_contract: Address::ZERO,
            chain_id: 4217,
            deposit: 100,
            cumulative_amount: 20,
            accepted_cumulative: 20,
            max_cumulative_spend: None,
            server_spent: 10,
        };

        let err = apply_response_receipt_from_headers(&response, &mut state, "open response", true)
            .expect_err("strict mode should reject invalid spent");
        assert!(err.to_string().contains("payment-receipt.spent"));

        server.abort();
        let _ = server.await;
    }
}
