//! SSE streaming for session payments.
//!
//! Handles Server-Sent Events (SSE) response streams with mid-stream
//! voucher top-ups and retry logic for lost server notifications.

use std::{
    collections::HashSet,
    io::Write,
    sync::{LazyLock, Mutex},
    time::Duration,
};

use mpp::server::sse::{parse_event, SseEvent};
use tokio::sync::mpsc;

#[cfg(test)]
use tokio::sync::Mutex as AsyncMutex;

use super::{
    error_map::payment_rejected_from_body,
    flow::validate_request_spend_limit,
    new_idempotency_key, open,
    persist::{persist_channel_cumulative_floor, persist_session},
    receipt::{
        apply_receipt_amounts_strict, invalid_payment_receipt_error, missing_payment_receipt_error,
        parse_validated_session_receipt_header, protocol_spent_error,
        validate_receipt_spent_strict, validate_session_receipt_fields,
    },
    voucher::{build_top_up_payload, build_voucher_credential},
    ChannelContext, ChannelState,
};
use tempo_common::{
    cli::terminal::sanitize_for_terminal,
    error::{NetworkError, PaymentError, TempoError},
    payment::{parse_problem_details, SessionProblemType},
};

fn protocol_value_error(field: &'static str, value: &str) -> TempoError {
    let safe_value = sanitize_for_terminal(value);
    PaymentError::PaymentRejected {
        reason: format!(
            "Malformed payment protocol field: {field} must be an integer amount (got '{safe_value}')"
        ),
        status_code: 502,
    }
    .into()
}

fn ensure_debug_log_boundary(token_line_open: &mut bool) {
    if *token_line_open {
        eprintln!();
        *token_line_open = false;
    }
}

fn parse_protocol_u128(value: &str, field: &'static str) -> Result<u128, TempoError> {
    value
        .trim()
        .parse::<u128>()
        .map_err(|_| protocol_value_error(field, value))
}

fn preserve_state_for_top_up_receipt_failure(
    ctx: &ChannelContext<'_>,
    state: &mut ChannelState,
    additional_deposit: u128,
) {
    // The top-up request already returned a successful paid response. Persist a
    // conservative deposit floor so strict receipt failures do not lose track
    // of potentially funded on-chain state.
    state.deposit = state.deposit.saturating_add(additional_deposit);
    if let Err(source) = persist_session(ctx, state) {
        tracing::warn!(
            error = %source,
            channel_id = %format!("{:#x}", state.channel_id),
            "Failed to preserve session state after strict top-up receipt failure"
        );
    }
}

fn parse_protocol_channel_id(
    value: &str,
    field: &'static str,
) -> Result<alloy::primitives::B256, TempoError> {
    value.trim().parse::<alloy::primitives::B256>().map_err(|_| {
        let safe_value = sanitize_for_terminal(value);
        PaymentError::PaymentRejected {
            reason: format!(
                "Malformed payment protocol field: {field} must be a bytes32 channel ID (got '{safe_value}')"
            ),
            status_code: 502,
        }
        .into()
    })
}

async fn send_top_up(
    ctx: &ChannelContext<'_>,
    client: &reqwest::Client,
    state: &mut ChannelState,
    additional_deposit: u128,
    idempotency_key: &str,
) -> Result<(), TempoError> {
    let calls = if state.is_tip1034() {
        let descriptor =
            state
                .descriptor
                .as_ref()
                .ok_or_else(|| PaymentError::ChallengeSchema {
                    context: "TIP-1034 topUp",
                    reason: "missing channel descriptor".to_string(),
                })?;
        tempo_common::session::build_tip1034_top_up_calls(descriptor, additional_deposit)?
    } else {
        tempo_common::session::build_top_up_calls(
            ctx.token,
            state.escrow_contract,
            state.channel_id,
            additional_deposit,
        )
    };
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
    let payload = build_top_up_payload(
        state.channel_id,
        tx_hex,
        state.descriptor.clone(),
        additional_deposit,
    );
    let credential =
        mpp::PaymentCredential::with_source(ctx.echo.clone(), ctx.did.to_string(), payload);
    let auth = mpp::format_authorization(&credential).map_err(|source| {
        PaymentError::ChallengeFormatSource {
            context: "topUp credential",
            source: Box::new(source),
        }
    })?;
    let response = client
        .post(ctx.url)
        .header("Authorization", auth)
        .header("Idempotency-Key", idempotency_key)
        .send()
        .await
        .map_err(NetworkError::Reqwest)?;
    let response = crate::http::HttpResponse::from_reqwest(response).await?;
    if response.status_code >= 400 {
        let body = response.body_string().unwrap_or_default();
        return Err(payment_rejected_from_body(response.status_code, &body));
    }

    match response.header("payment-receipt") {
        Some(receipt_header) => {
            match parse_validated_session_receipt_header(receipt_header, state.channel_id) {
                Ok(receipt) => {
                    if let Some(reason) = receipt.spent_parse_error {
                        let error = protocol_spent_error(reason);
                        preserve_state_for_top_up_receipt_failure(ctx, state, additional_deposit);
                        return Err(error);
                    }
                    apply_receipt_amounts_strict(
                        state,
                        receipt.accepted_cumulative,
                        receipt.server_spent,
                    )?;
                    persist_session(ctx, state)?;
                }
                Err(reason) => {
                    let error = invalid_payment_receipt_error("topUp response", &reason);
                    preserve_state_for_top_up_receipt_failure(ctx, state, additional_deposit);
                    return Err(error);
                }
            }
        }
        None => {
            let error = missing_payment_receipt_error("topUp response");
            preserve_state_for_top_up_receipt_failure(ctx, state, additional_deposit);
            return Err(error);
        }
    }

    Ok(())
}

fn stream_idle_timeout_error(timeout: Duration) -> TempoError {
    PaymentError::SessionStreamIdleTimeout {
        timeout_secs: timeout.as_secs().max(1),
    }
    .into()
}

fn stream_retry_exhausted_error(max_retries: u32) -> TempoError {
    PaymentError::SessionVoucherRetryExhausted { max_retries }.into()
}

fn stream_incomplete_error(reason: &'static str) -> TempoError {
    PaymentError::SessionStreamIncomplete {
        reason: reason.to_string(),
    }
    .into()
}

fn next_voucher_stall_timeout(current: Duration, normal_timeout: Duration) -> Duration {
    current.saturating_mul(2).min(normal_timeout)
}

fn parse_env_u32(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
}

fn parse_env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
}

#[derive(Debug, Clone, Copy)]
struct VoucherRetryPolicy {
    max_voucher_retries: u32,
    normal_timeout: Duration,
    base_stall_timeout: Duration,
}

#[derive(Debug)]
struct VoucherRetryCoordinator {
    policy: VoucherRetryPolicy,
    pending_voucher_auth: Option<String>,
    pending_voucher_idempotency_key: Option<String>,
    retry_count: u32,
    current_stall_timeout: Duration,
}

#[derive(Debug, Clone)]
struct VoucherRetryAttempt {
    auth: String,
    idempotency_key: String,
    retry_count: u32,
    max_voucher_retries: u32,
}

#[derive(Debug, Clone)]
enum TimeoutTransition {
    NoPendingVoucher,
    Retry(VoucherRetryAttempt),
    RetryExhausted { max_voucher_retries: u32 },
}

impl VoucherRetryCoordinator {
    fn new(policy: VoucherRetryPolicy) -> Self {
        Self {
            policy,
            pending_voucher_auth: None,
            pending_voucher_idempotency_key: None,
            retry_count: 0,
            current_stall_timeout: policy.base_stall_timeout,
        }
    }

    fn stream_read_timeout(&self) -> Duration {
        if self.pending_voucher_auth.is_some() {
            self.current_stall_timeout
        } else {
            self.policy.normal_timeout
        }
    }

    fn clear_pending_voucher(&mut self) {
        self.pending_voucher_auth = None;
        self.pending_voucher_idempotency_key = None;
        self.retry_count = 0;
        self.current_stall_timeout = self.policy.base_stall_timeout;
    }

    fn begin_pending_voucher(&mut self, auth: String, idempotency_key: String) {
        self.pending_voucher_auth = Some(auth);
        self.pending_voucher_idempotency_key = Some(idempotency_key);
        self.retry_count = 0;
        self.current_stall_timeout = self.policy.base_stall_timeout;
    }

    fn on_stream_timeout(&mut self) -> TimeoutTransition {
        let Some(auth) = self.pending_voucher_auth.clone() else {
            return TimeoutTransition::NoPendingVoucher;
        };

        self.retry_count = self.retry_count.saturating_add(1);
        if self.retry_count > self.policy.max_voucher_retries {
            return TimeoutTransition::RetryExhausted {
                max_voucher_retries: self.policy.max_voucher_retries,
            };
        }

        let idempotency_key = self
            .pending_voucher_idempotency_key
            .clone()
            .unwrap_or_default();
        let attempt = VoucherRetryAttempt {
            auth,
            idempotency_key,
            retry_count: self.retry_count,
            max_voucher_retries: self.policy.max_voucher_retries,
        };
        self.current_stall_timeout =
            next_voucher_stall_timeout(self.current_stall_timeout, self.policy.normal_timeout);

        TimeoutTransition::Retry(attempt)
    }
}

fn voucher_retry_policy() -> VoucherRetryPolicy {
    const DEFAULT_MAX_VOUCHER_RETRIES: u32 = 5;
    const DEFAULT_NORMAL_TIMEOUT_MS: u64 = 30_000;
    const DEFAULT_VOUCHER_STALL_TIMEOUT_MS: u64 = 3_000;

    let max_voucher_retries =
        parse_env_u32("TEMPO_SESSION_MAX_VOUCHER_RETRIES").unwrap_or(DEFAULT_MAX_VOUCHER_RETRIES);
    let normal_timeout_ms =
        parse_env_u64("TEMPO_SESSION_NORMAL_TIMEOUT_MS").unwrap_or(DEFAULT_NORMAL_TIMEOUT_MS);
    let stall_timeout_ms =
        parse_env_u64("TEMPO_SESSION_STALL_TIMEOUT_MS").unwrap_or(DEFAULT_VOUCHER_STALL_TIMEOUT_MS);

    let normal_timeout = Duration::from_millis(normal_timeout_ms.max(1));
    let base_stall_timeout = Duration::from_millis(stall_timeout_ms.max(1)).min(normal_timeout);

    VoucherRetryPolicy {
        max_voucher_retries,
        normal_timeout,
        base_stall_timeout,
    }
}

fn build_voucher_transport_client(base: &reqwest::Client) -> reqwest::Client {
    // Keep voucher transport policy aligned with the primary request client
    // so proxy/TLS/timeouts behave consistently across both paths.
    base.clone()
}

fn parse_problem_accepted_cumulative(
    problem: &tempo_common::payment::ProblemDetails,
) -> Option<u128> {
    match problem.extensions.get("acceptedCumulative") {
        Some(serde_json::Value::String(value)) => value.parse::<u128>().ok(),
        Some(serde_json::Value::Number(value)) => value.to_string().parse::<u128>().ok(),
        _ => None,
    }
}

async fn fetch_fresh_session_echo(
    url: &str,
    client: &reqwest::Client,
) -> Result<mpp::ChallengeEcho, TempoError> {
    let response = client
        .head(url)
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
        .map_err(|err| tempo_common::payment::map_mpp_validation_error(err, &challenge))?;

    Ok(challenge.to_echo())
}

#[derive(Debug)]
struct VoucherTaskResult {
    _idempotency_key: String,
    result: Result<(), TempoError>,
}

fn drain_voucher_results(
    result_rx: &mut mpsc::UnboundedReceiver<VoucherTaskResult>,
    in_flight_voucher_tasks: &mut usize,
) -> Result<(), TempoError> {
    while let Ok(task_result) = result_rx.try_recv() {
        *in_flight_voucher_tasks = in_flight_voucher_tasks.saturating_sub(1);
        task_result.result?;
    }
    Ok(())
}

async fn wait_for_pending_voucher_results(
    result_rx: &mut mpsc::UnboundedReceiver<VoucherTaskResult>,
    in_flight_voucher_tasks: &mut usize,
    max_wait: Duration,
) -> Result<(), TempoError> {
    if *in_flight_voucher_tasks == 0 {
        return Ok(());
    }

    let wait_result = tokio::time::timeout(max_wait, async {
        while *in_flight_voucher_tasks > 0 {
            let Some(task_result) = result_rx.recv().await else {
                *in_flight_voucher_tasks = 0;
                break;
            };
            *in_flight_voucher_tasks = in_flight_voucher_tasks.saturating_sub(1);
            task_result.result?;
        }
        Ok::<(), TempoError>(())
    })
    .await;

    match wait_result {
        Ok(result) => result,
        Err(_) => Err(stream_incomplete_error(
            "stream ended before voucher update tasks completed",
        )),
    }
}

struct VoucherSubmitContext<'a> {
    url: &'a str,
    signer: &'a tempo_common::keys::Signer,
    did: &'a str,
    debug_enabled: bool,
    client: &'a reqwest::Client,
    state: &'a ChannelState,
    auth: &'a str,
    idempotency_key: &'a str,
}

static HEAD_UNSUPPORTED_ORIGINS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn voucher_origin_key(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default()?;
    Some(format!("{}://{}:{}", parsed.scheme(), host, port))
}

fn is_head_known_unsupported(url: &str) -> bool {
    let Some(origin_key) = voucher_origin_key(url) else {
        return false;
    };
    let guard = HEAD_UNSUPPORTED_ORIGINS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.contains(&origin_key)
}

fn remember_head_unsupported(url: &str) {
    let Some(origin_key) = voucher_origin_key(url) else {
        return;
    };
    let mut guard = HEAD_UNSUPPORTED_ORIGINS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.insert(origin_key);
}

#[cfg(test)]
fn clear_head_unsupported_cache() {
    let mut guard = HEAD_UNSUPPORTED_ORIGINS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.clear();
}

#[cfg(test)]
static HEAD_CACHE_TEST_MUTEX: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

#[cfg(test)]
async fn lock_head_cache_tests() -> tokio::sync::MutexGuard<'static, ()> {
    HEAD_CACHE_TEST_MUTEX.lock().await
}

async fn post_voucher_update(
    ctx: &VoucherSubmitContext<'_>,
) -> Result<reqwest::Response, TempoError> {
    Ok(ctx
        .client
        .post(ctx.url)
        .header("Authorization", ctx.auth)
        .header("Idempotency-Key", ctx.idempotency_key)
        .send()
        .await
        .map_err(NetworkError::Reqwest)?)
}

const fn should_fallback_to_post(status_code: u16) -> bool {
    matches!(status_code, 404 | 405 | 501)
}

async fn submit_voucher_update(ctx: VoucherSubmitContext<'_>) -> Result<(), TempoError> {
    let response = if is_head_known_unsupported(ctx.url) {
        post_voucher_update(&ctx).await?
    } else {
        let head_response = ctx
            .client
            .head(ctx.url)
            .header("Authorization", ctx.auth)
            .header("Idempotency-Key", ctx.idempotency_key)
            .send()
            .await;

        match head_response {
            Ok(resp) if should_fallback_to_post(resp.status().as_u16()) => {
                remember_head_unsupported(ctx.url);
                if ctx.debug_enabled {
                    eprintln!(
                        "[voucher HEAD unsupported ({}) — falling back to POST]",
                        resp.status()
                    );
                }
                post_voucher_update(&ctx).await?
            }
            Ok(resp) => resp,
            Err(_) => {
                if ctx.debug_enabled {
                    eprintln!("[voucher HEAD transport failure — falling back to POST]");
                }
                post_voucher_update(&ctx).await?
            }
        }
    };

    if response.status().is_success() {
        match response
            .headers()
            .get("payment-receipt")
            .and_then(|value| value.to_str().ok())
        {
            Some(receipt_header) => {
                match parse_validated_session_receipt_header(receipt_header, ctx.state.channel_id) {
                    Ok(receipt) => {
                        if let Some(reason) = receipt.spent_parse_error {
                            return Err(protocol_spent_error(reason));
                        }
                        validate_receipt_spent_strict(
                            receipt.accepted_cumulative,
                            receipt.server_spent,
                        )?;
                        persist_channel_cumulative_floor(
                            ctx.state.channel_id,
                            receipt.accepted_cumulative,
                        )?;
                    }
                    Err(reason) => {
                        return Err(invalid_payment_receipt_error("voucher response", &reason))
                    }
                }
            }
            None => return Err(missing_payment_receipt_error("voucher response")),
        }
        return Ok(());
    }

    let status_code = response.status().as_u16();
    if ctx.debug_enabled {
        eprintln!("[voucher update rejected: HTTP {status_code}]");
    }

    // Treat 5xx as transient for voucher updates. Some providers can process
    // the voucher but still surface an internal error response. The stream
    // stall retry coordinator will re-post if progress does not resume.
    if status_code >= 500 {
        return Ok(());
    }

    let body = match tokio::time::timeout(Duration::from_secs(2), response.text()).await {
        Ok(Ok(text)) => text,
        _ => String::new(),
    };

    if let Some(problem) = parse_problem_details(&body) {
        if problem.classify() == SessionProblemType::DeltaTooSmall {
            if let Some(accepted_cumulative) = parse_problem_accepted_cumulative(&problem) {
                persist_channel_cumulative_floor(ctx.state.channel_id, accepted_cumulative)?;
            }
            return Ok(());
        }

        if problem.classify() == SessionProblemType::ChallengeNotFound {
            let fresh_echo = fetch_fresh_session_echo(ctx.url, ctx.client).await?;
            let voucher =
                build_voucher_credential(ctx.signer, &fresh_echo, ctx.did, ctx.state).await?;
            let fresh_auth = mpp::format_authorization(&voucher).map_err(|source| {
                PaymentError::ChallengeFormatSource {
                    context: "voucher credential",
                    source: Box::new(source),
                }
            })?;

            let retry_response = ctx
                .client
                .post(ctx.url)
                .header("Authorization", fresh_auth)
                .header("Idempotency-Key", ctx.idempotency_key)
                .send()
                .await
                .map_err(NetworkError::Reqwest)?;

            if retry_response.status().is_success() {
                if let Some(receipt_header) = retry_response
                    .headers()
                    .get("payment-receipt")
                    .and_then(|value| value.to_str().ok())
                {
                    match parse_validated_session_receipt_header(
                        receipt_header,
                        ctx.state.channel_id,
                    ) {
                        Ok(receipt) => {
                            if let Some(reason) = receipt.spent_parse_error {
                                return Err(protocol_spent_error(reason));
                            }
                            validate_receipt_spent_strict(
                                receipt.accepted_cumulative,
                                receipt.server_spent,
                            )?;
                            persist_channel_cumulative_floor(
                                ctx.state.channel_id,
                                receipt.accepted_cumulative,
                            )?;
                        }
                        Err(reason) => {
                            return Err(invalid_payment_receipt_error("voucher response", &reason));
                        }
                    }
                } else {
                    return Err(missing_payment_receipt_error("voucher response"));
                }
                return Ok(());
            }

            let retry_status = retry_response.status().as_u16();
            let retry_body = retry_response.text().await.unwrap_or_default();
            return Err(payment_rejected_from_body(retry_status, &retry_body));
        }
    }

    Err(payment_rejected_from_body(status_code, &body))
}

/// Post a voucher to the server in a background task.
///
/// We MUST NOT await the response inline because the server may respond
/// with a streaming body (treating the POST as a new chat request).
/// Awaiting would deadlock: the server waits for us to read the SSE
/// stream, and we wait for the POST response.
fn post_voucher(
    ctx: &ChannelContext<'_>,
    client: &reqwest::Client,
    auth: &str,
    idempotency_key: &str,
    state: &ChannelState,
    result_tx: mpsc::UnboundedSender<VoucherTaskResult>,
) {
    let signer = ctx.signer.clone();
    let did = ctx.did.to_string();
    let debug_enabled = ctx.http.debug_enabled();
    let url = ctx.url.to_string();

    let state = ChannelState {
        channel_id: state.channel_id,
        escrow_contract: state.escrow_contract,
        chain_id: state.chain_id,
        deposit: state.deposit,
        cumulative_amount: state.cumulative_amount,
        accepted_cumulative: state.accepted_cumulative,
        max_cumulative_spend: state.max_cumulative_spend,
        server_spent: state.server_spent,
        session_protocol: state.session_protocol.clone(),
        descriptor: state.descriptor.clone(),
    };

    let client = client.clone();
    let auth = auth.to_string();
    let idempotency_key = idempotency_key.to_string();
    tokio::spawn(async move {
        let result = submit_voucher_update(VoucherSubmitContext {
            url: &url,
            signer: &signer,
            did: &did,
            debug_enabled,
            client: &client,
            state: &state,
            auth: &auth,
            idempotency_key: &idempotency_key,
        })
        .await;
        let _ = result_tx.send(VoucherTaskResult {
            _idempotency_key: idempotency_key,
            result,
        });
    });
}

/// Stream SSE events from a response, handling voucher top-ups mid-stream.
///
/// Persists cumulative amount updates during streaming so that if the
/// process is interrupted, the session record reflects the last voucher sent.
///
/// The server has a known race condition where its `wait_for_update` notification
/// can be lost (`tokio::sync::Notify` without permit storage). When a voucher POST
/// arrives but the server hasn't started awaiting yet, the notification is dropped
/// and the stream stalls. We work around this by re-posting the same voucher if
/// no progress is seen within a short timeout after the last need-voucher event.
pub(super) async fn stream_sse_response(
    ctx: &ChannelContext<'_>,
    state: &mut ChannelState,
    response: reqwest::Response,
) -> Result<(), TempoError> {
    let runtime = ctx.http;
    let mut response = response;
    let mut buffer = String::new();
    let mut token_count: u64 = 0;
    let mut stdout = std::io::stdout();
    let mut saw_response_receipt = false;
    let mut token_line_open = false;

    let mut stream_done = false;

    if response.status().is_success() {
        if let Some(receipt_header) = response
            .headers()
            .get("payment-receipt")
            .and_then(|value| value.to_str().ok())
        {
            saw_response_receipt = true;
            match parse_validated_session_receipt_header(receipt_header, state.channel_id) {
                Ok(receipt) => {
                    if let Some(reason) = receipt.spent_parse_error {
                        return Err(protocol_spent_error(reason));
                    }
                    apply_receipt_amounts_strict(
                        state,
                        receipt.accepted_cumulative,
                        receipt.server_spent,
                    )?;
                    persist_session(ctx, state)?;
                }
                Err(reason) => {
                    return Err(invalid_payment_receipt_error(
                        "SSE response header",
                        &reason,
                    ))
                }
            }
        }
    }

    // Cap SSE buffer to prevent unbounded growth from malformed streams
    // that never emit the \n\n event delimiter.
    const MAX_BUFFER_SIZE: usize = 4 * 1024 * 1024; // 4 MB

    // Use a dedicated transport client for voucher/top-up updates.
    let voucher_client = build_voucher_transport_client(ctx.reqwest_client);
    let (voucher_result_tx, mut voucher_result_rx) = mpsc::unbounded_channel::<VoucherTaskResult>();
    let mut in_flight_voucher_tasks: usize = 0;

    let mut retry_coordinator = VoucherRetryCoordinator::new(voucher_retry_policy());

    loop {
        drain_voucher_results(&mut voucher_result_rx, &mut in_flight_voucher_tasks)?;

        if stream_done {
            break;
        }

        let timeout = retry_coordinator.stream_read_timeout();

        let chunk = match tokio::time::timeout(timeout, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => break, // stream ended
            Ok(Err(source)) => return Err(NetworkError::Reqwest(source).into()),
            Err(_) => match retry_coordinator.on_stream_timeout() {
                TimeoutTransition::RetryExhausted {
                    max_voucher_retries,
                } => {
                    if runtime.debug_enabled() {
                        ensure_debug_log_boundary(&mut token_line_open);
                        eprintln!(
                            "[stream stall — voucher not accepted after {max_voucher_retries} retries]"
                        );
                    }
                    return Err(stream_retry_exhausted_error(max_voucher_retries));
                }
                TimeoutTransition::Retry(attempt) => {
                    if runtime.debug_enabled() {
                        ensure_debug_log_boundary(&mut token_line_open);
                        eprintln!(
                            "[re-posting voucher (retry {}/{})]",
                            attempt.retry_count, attempt.max_voucher_retries
                        );
                    }
                    post_voucher(
                        ctx,
                        &voucher_client,
                        &attempt.auth,
                        &attempt.idempotency_key,
                        state,
                        voucher_result_tx.clone(),
                    );
                    in_flight_voucher_tasks = in_flight_voucher_tasks.saturating_add(1);
                    continue;
                }
                TimeoutTransition::NoPendingVoucher => {
                    if runtime.debug_enabled() {
                        ensure_debug_log_boundary(&mut token_line_open);
                        eprintln!(
                            "[stream timeout — no data for {}s]",
                            retry_coordinator.policy.normal_timeout.as_secs()
                        );
                    }
                    return Err(stream_idle_timeout_error(
                        retry_coordinator.policy.normal_timeout,
                    ));
                }
            },
        };
        let chunk_str = String::from_utf8_lossy(&chunk);
        // Normalize \r\n to \n so SSE event boundary detection works with
        // servers/proxies that emit CRLF line endings.
        if chunk_str.contains('\r') {
            buffer.push_str(&chunk_str.replace("\r\n", "\n"));
        } else {
            buffer.push_str(&chunk_str);
        }

        if buffer.len() > MAX_BUFFER_SIZE {
            return Err(tempo_common::error::NetworkError::ResponseSchema {
                context: "SSE stream",
                reason: format!("buffer exceeded {MAX_BUFFER_SIZE} bytes without a complete event"),
            }
            .into());
        }

        while let Some(pos) = buffer.find("\n\n") {
            let event_str: String = buffer.drain(..pos + 2).collect();

            if let Some(event) = parse_event(&event_str) {
                match event {
                    SseEvent::Message(data) => {
                        // Any message means the voucher was accepted
                        retry_coordinator.clear_pending_voucher();

                        if data.trim() == "[DONE]" {
                            stream_done = true;
                            break;
                        }
                        let (content, finished) = parse_sse_chunk(&data);
                        if let Some(content) = content {
                            token_count += 1;
                            write!(stdout, "{content}")?;
                            stdout.flush()?;
                            token_line_open = true;
                        }
                        if finished {
                            stream_done = true;
                            break;
                        }
                    }
                    SseEvent::PaymentNeedVoucher(nv) => {
                        let event_channel_id = parse_protocol_channel_id(
                            &nv.channel_id,
                            "payment-need-voucher.channelId",
                        )?;
                        if event_channel_id != state.channel_id {
                            return Err(PaymentError::PaymentRejected {
                                reason: format!(
                                    "Malformed payment protocol field: payment-need-voucher.channelId mismatch (expected {:#x}, got {event_channel_id:#x})",
                                    state.channel_id
                                ),
                                status_code: 502,
                            }
                            .into());
                        }

                        let required = parse_protocol_u128(
                            &nv.required_cumulative,
                            "payment-need-voucher.requiredCumulative",
                        )?;
                        let accepted = parse_protocol_u128(
                            &nv.accepted_cumulative,
                            "payment-need-voucher.acceptedCumulative",
                        )?;
                        let on_chain_deposit =
                            parse_protocol_u128(&nv.deposit, "payment-need-voucher.deposit")?;

                        let next_cumulative = state.cumulative_amount.max(accepted).max(required);
                        validate_request_spend_limit(state, ctx.network_id, next_cumulative)?;

                        let mut effective_deposit = on_chain_deposit;
                        if required > on_chain_deposit {
                            let top_up_idempotency_key = new_idempotency_key();
                            let additional_deposit =
                                if let Some(max_cumulative_spend) = state.max_cumulative_spend {
                                    max_cumulative_spend.saturating_sub(on_chain_deposit)
                                } else {
                                    (required - on_chain_deposit).max(ctx.top_up_deposit)
                                };
                            if additional_deposit == 0 {
                                return Err(PaymentError::DepositInsufficient {
                                    deposit: tempo_common::cli::format::format_token_amount(
                                        on_chain_deposit,
                                        ctx.network_id,
                                    ),
                                    amount: tempo_common::cli::format::format_token_amount(
                                        required,
                                        ctx.network_id,
                                    ),
                                }
                                .into());
                            }
                            if runtime.debug_enabled() {
                                ensure_debug_log_boundary(&mut token_line_open);
                                eprintln!(
                                    "[channel top-up: required={required} deposit={on_chain_deposit} additional={additional_deposit}]"
                                );
                            }
                            send_top_up(
                                ctx,
                                &voucher_client,
                                state,
                                additional_deposit,
                                &top_up_idempotency_key,
                            )
                            .await?;
                            effective_deposit = on_chain_deposit.saturating_add(additional_deposit);
                            state.deposit = state.deposit.max(effective_deposit);
                        }

                        if effective_deposit < state.cumulative_amount {
                            return Err(PaymentError::PaymentRejected {
                                reason: format!(
                                    "Malformed payment protocol field: payment-need-voucher.deposit below local cumulative floor (deposit={effective_deposit}, localCumulative={})",
                                    state.cumulative_amount
                                ),
                                status_code: 502,
                            }
                            .into());
                        }

                        // Use the server's required amount, clamped to our known
                        // channel deposit to prevent a malicious server from
                        // coercing an overly large voucher.
                        let authorize_amount = next_cumulative.min(effective_deposit);

                        // Sign the voucher for the authorized amount (monotonic: never decrease)
                        let signing_cumulative = state.cumulative_amount.max(authorize_amount);
                        state.cumulative_amount = signing_cumulative;
                        let voucher =
                            build_voucher_credential(ctx.signer, ctx.echo, ctx.did, state).await?;
                        let auth = mpp::format_authorization(&voucher).map_err(|source| {
                            PaymentError::ChallengeFormatSource {
                                context: "voucher credential",
                                source: Box::new(source),
                            }
                        })?;
                        let voucher_idempotency_key = new_idempotency_key();

                        post_voucher(
                            ctx,
                            &voucher_client,
                            &auth,
                            &voucher_idempotency_key,
                            state,
                            voucher_result_tx.clone(),
                        );
                        in_flight_voucher_tasks = in_flight_voucher_tasks.saturating_add(1);

                        // For our persisted record, keep the exact required amount
                        // (clamped to deposit) so cooperative close can match the
                        // server's expectation precisely.
                        // Enforce monotonicity: never decrease the cumulative amount.
                        let persisted_cumulative =
                            signing_cumulative.max(next_cumulative.min(effective_deposit));
                        state.cumulative_amount = persisted_cumulative;
                        persist_session(ctx, state)?;

                        // Track this voucher for retry if the server stalls.
                        retry_coordinator.begin_pending_voucher(auth, voucher_idempotency_key);
                    }
                    SseEvent::PaymentReceipt(receipt) => {
                        retry_coordinator.clear_pending_voucher();
                        saw_response_receipt = true;
                        match validate_session_receipt_fields(&receipt, state.channel_id) {
                            Ok(accepted_cumulative) => {
                                let spent = match receipt.spent.trim().parse::<u128>() {
                                    Ok(value) => Some(value),
                                    Err(_) => {
                                        return Err(protocol_spent_error(format!(
                                            "must be an integer amount (got '{}')",
                                            sanitize_for_terminal(&receipt.spent)
                                        )));
                                    }
                                };

                                apply_receipt_amounts_strict(state, accepted_cumulative, spent)?;
                                persist_session(ctx, state)?;
                            }
                            Err(reason) => {
                                return Err(invalid_payment_receipt_error(
                                    "SSE payment-receipt event",
                                    &reason,
                                ));
                            }
                        }
                        if runtime.log_enabled() {
                            ensure_debug_log_boundary(&mut token_line_open);
                            eprintln!();
                            eprintln!("Stream receipt:");
                            let safe_channel = sanitize_for_terminal(&receipt.channel_id);
                            let safe_spent = sanitize_for_terminal(&receipt.spent);
                            eprintln!("  Channel: {safe_channel}");
                            eprintln!("  Spent: {safe_spent}");
                            if let Some(units) = receipt.units {
                                let safe_units = sanitize_for_terminal(&units.to_string());
                                eprintln!("  Units: {safe_units}");
                            }
                            if let Some(ref tx) = receipt.tx_hash {
                                let safe_tx = sanitize_for_terminal(tx);
                                eprintln!("  TX: {safe_tx}");
                            }
                        }
                        // Receipt signals stream completion
                        stream_done = true;
                        break;
                    }
                }
            }
        }
    }

    drain_voucher_results(&mut voucher_result_rx, &mut in_flight_voucher_tasks)?;
    // Only block for pending voucher results if the stream ended without a
    // completion marker — those tasks may carry error information we need.
    // When the stream completed normally (stream_done), any remaining voucher
    // tasks are stale and blocking on them delays exit unnecessarily.
    if !stream_done {
        wait_for_pending_voucher_results(
            &mut voucher_result_rx,
            &mut in_flight_voucher_tasks,
            retry_coordinator.policy.normal_timeout,
        )
        .await?;
    }

    if response.status().is_success() && !stream_done && !saw_response_receipt {
        return Err(stream_incomplete_error(
            "stream ended without completion marker or payment receipt",
        ));
    }

    if response.status().is_success() && !saw_response_receipt {
        if stream_done {
            // Compatibility: some providers complete streaming without sending
            // a payment-receipt. We already persist voucher cumulative updates
            // and fail closed when stream progress is incomplete.
            if runtime.debug_enabled() {
                ensure_debug_log_boundary(&mut token_line_open);
                eprintln!("[SSE stream completed without payment-receipt]");
            }
        } else {
            return Err(missing_payment_receipt_error("SSE response"));
        }
    }

    writeln!(stdout)?;

    if runtime.log_enabled() {
        eprintln!("Tokens streamed: {token_count}");
        let cumulative_display =
            tempo_common::cli::format::format_token_amount(state.cumulative_amount, ctx.network_id);
        eprintln!("Voucher cumulative: {cumulative_display}");
    }

    Ok(())
}

/// Parse an SSE data chunk, extracting token content and finish status.
///
/// Returns `(content, finished)`:
/// - `content`: The text token from an `OpenAI` `delta.content` field, or the raw
///   text for non-JSON SSE. `None` for role-only deltas or empty content.
/// - `finished`: `true` if `finish_reason` is non-null (model done generating).
fn parse_sse_chunk(raw: &str) -> (Option<String>, bool) {
    let trimmed = raw.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let choice = v.get("choices").and_then(|c| c.get(0));
        let finished = choice
            .and_then(|c| c.get("finish_reason"))
            .is_some_and(|r| !r.is_null());
        let content = choice
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        (content, finished)
    } else {
        // Not JSON — return raw content as-is (plain text SSE)
        (Some(trimmed.to_string()), false)
    }
}

#[cfg(test)]
mod tests {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    };

    use axum::{
        body::{Body, Bytes},
        http::StatusCode,
        routing::{get, head, post},
        Router,
    };
    use futures::StreamExt;

    use crate::http::{HttpClient, HttpRequestPlan};

    use super::*;

    const TEST_PRIVATE_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn test_signer() -> tempo_common::keys::Signer {
        let signer = tempo_common::keys::parse_private_key_signer(TEST_PRIVATE_KEY).unwrap();
        tempo_common::keys::Signer {
            from: signer.address(),
            signer,
            signing_mode: mpp::client::tempo::signing::TempoSigningMode::Direct,
            stored_key_authorization: None,
        }
    }

    fn test_echo() -> mpp::ChallengeEcho {
        mpp::ChallengeEcho {
            id: "test-challenge".to_string(),
            realm: "test".to_string(),
            method: mpp::protocol::core::MethodName::from("tempo"),
            intent: mpp::protocol::core::IntentName::from("session"),
            request: mpp::Base64UrlJson::from_raw("e30"),
            expires: None,
            digest: None,
            opaque: None,
        }
    }

    fn test_http_client() -> HttpClient {
        let plan = HttpRequestPlan {
            method: reqwest::Method::POST,
            ..Default::default()
        };
        HttpClient::new(
            plan,
            tempo_common::cli::Verbosity {
                level: 0,
                show_output: false,
            },
            None,
            false,
            None,
        )
        .unwrap()
    }

    fn test_state(channel_id: alloy::primitives::B256) -> ChannelState {
        ChannelState {
            channel_id,
            escrow_contract: "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            chain_id: 4217,
            deposit: 100,
            cumulative_amount: 10,
            accepted_cumulative: 0,
            max_cumulative_spend: None,
            server_spent: 0,
            session_protocol: mpp::protocol::methods::tempo::session::SESSION_PROTOCOL_LEGACY
                .to_string(),
            descriptor: None,
        }
    }

    fn encode_session_receipt_header(
        channel_id: alloy::primitives::B256,
        accepted_cumulative: u128,
        spent: u128,
    ) -> String {
        let receipt = mpp::protocol::methods::tempo::SessionReceipt {
            method: "tempo".to_string(),
            intent: "session".to_string(),
            status: "success".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            reference: format!("{channel_id:#x}"),
            challenge_id: "challenge-123".to_string(),
            channel_id: format!("{channel_id:#x}"),
            accepted_cumulative: accepted_cumulative.to_string(),
            spent: spent.to_string(),
            units: Some(1),
            tx_hash: Some(format!("{:#x}", alloy::primitives::B256::from([0x11; 32]))),
        };
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&receipt).unwrap())
    }

    async fn spawn_test_server(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/voucher"), server)
    }

    fn env_lock() -> &'static Mutex<()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn parse_protocol_u128_rejects_invalid_value() {
        let err = parse_protocol_u128("abc", "field").unwrap_err();
        assert!(err.to_string().contains("Malformed payment protocol field"));
    }

    #[test]
    fn parse_protocol_u128_error_sanitizes_control_chars() {
        let err = parse_protocol_u128("bad\u{1b}[31m", "field").unwrap_err();
        let msg = err.to_string();
        assert!(!msg.chars().any(char::is_control));
        assert!(msg.contains("bad[31m"));
    }

    #[test]
    fn should_fallback_to_post_only_for_expected_statuses() {
        assert!(should_fallback_to_post(404));
        assert!(should_fallback_to_post(405));
        assert!(should_fallback_to_post(501));
        assert!(!should_fallback_to_post(400));
        assert!(!should_fallback_to_post(500));
    }

    #[test]
    fn parse_protocol_u128_accepts_trimmed_integer() {
        let value = parse_protocol_u128(" 42 ", "field").unwrap();
        assert_eq!(value, 42);
    }

    #[test]
    fn parse_protocol_u128_rejects_empty_value() {
        let err = parse_protocol_u128("   ", "requiredCumulative").unwrap_err();
        assert!(err.to_string().contains("requiredCumulative"));
    }

    #[test]
    fn parse_protocol_u128_reports_specific_need_voucher_field_name() {
        let err =
            parse_protocol_u128("abc", "payment-need-voucher.acceptedCumulative").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("payment-need-voucher.acceptedCumulative"));
        assert!(msg.contains("must be an integer amount"));
    }

    #[test]
    fn parse_protocol_channel_id_rejects_invalid_value() {
        let err = parse_protocol_channel_id("0x1234", "field").unwrap_err();
        assert!(err.to_string().contains("bytes32 channel ID"));
    }

    #[test]
    fn parse_problem_accepted_cumulative_accepts_string_and_number() {
        let mut as_string = tempo_common::payment::ProblemDetails {
            problem_type: "https://example.com/problem".to_string(),
            title: None,
            status: Some(410),
            detail: None,
            required_top_up: None,
            channel_id: None,
            extensions: std::collections::BTreeMap::new(),
        };
        as_string.extensions.insert(
            "acceptedCumulative".to_string(),
            serde_json::Value::String("1234".to_string()),
        );
        assert_eq!(parse_problem_accepted_cumulative(&as_string), Some(1234));

        let mut as_number = as_string.clone();
        as_number
            .extensions
            .insert("acceptedCumulative".to_string(), serde_json::json!(5678));
        assert_eq!(parse_problem_accepted_cumulative(&as_number), Some(5678));

        let mut as_u64_max_number = as_string;
        as_u64_max_number.extensions.insert(
            "acceptedCumulative".to_string(),
            serde_json::json!(u64::MAX),
        );
        assert_eq!(
            parse_problem_accepted_cumulative(&as_u64_max_number),
            Some(u64::MAX as u128)
        );
    }

    #[test]
    fn parse_problem_accepted_cumulative_rejects_non_numeric_values() {
        let mut problem = tempo_common::payment::ProblemDetails {
            problem_type: "https://example.com/problem".to_string(),
            title: None,
            status: Some(410),
            detail: None,
            required_top_up: None,
            channel_id: None,
            extensions: std::collections::BTreeMap::new(),
        };
        problem.extensions.insert(
            "acceptedCumulative".to_string(),
            serde_json::Value::Bool(true),
        );
        assert_eq!(parse_problem_accepted_cumulative(&problem), None);
    }

    #[test]
    fn next_voucher_stall_timeout_doubles_and_caps() {
        let normal = Duration::from_secs(30);
        assert_eq!(
            next_voucher_stall_timeout(Duration::from_secs(3), normal),
            Duration::from_secs(6)
        );
        assert_eq!(
            next_voucher_stall_timeout(Duration::from_secs(20), normal),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn voucher_retry_policy_uses_defaults_without_env_overrides() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var("TEMPO_SESSION_MAX_VOUCHER_RETRIES");
        std::env::remove_var("TEMPO_SESSION_NORMAL_TIMEOUT_MS");
        std::env::remove_var("TEMPO_SESSION_STALL_TIMEOUT_MS");

        let policy = voucher_retry_policy();
        assert_eq!(policy.max_voucher_retries, 5);
        assert_eq!(policy.normal_timeout, Duration::from_millis(30_000));
        assert_eq!(policy.base_stall_timeout, Duration::from_millis(3_000));
    }

    #[test]
    fn voucher_retry_policy_applies_env_overrides_and_bounds() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var("TEMPO_SESSION_MAX_VOUCHER_RETRIES", "7");
        std::env::set_var("TEMPO_SESSION_NORMAL_TIMEOUT_MS", "10");
        std::env::set_var("TEMPO_SESSION_STALL_TIMEOUT_MS", "999");

        let policy = voucher_retry_policy();
        assert_eq!(policy.max_voucher_retries, 7);
        assert_eq!(policy.normal_timeout, Duration::from_millis(10));
        assert_eq!(
            policy.base_stall_timeout,
            Duration::from_millis(10),
            "stall timeout should be capped by normal timeout"
        );

        std::env::remove_var("TEMPO_SESSION_MAX_VOUCHER_RETRIES");
        std::env::remove_var("TEMPO_SESSION_NORMAL_TIMEOUT_MS");
        std::env::remove_var("TEMPO_SESSION_STALL_TIMEOUT_MS");
    }

    #[test]
    fn voucher_retry_coordinator_timeout_progression_and_exhaustion() {
        let policy = VoucherRetryPolicy {
            max_voucher_retries: 2,
            normal_timeout: Duration::from_millis(10),
            base_stall_timeout: Duration::from_millis(2),
        };
        let mut coordinator = VoucherRetryCoordinator::new(policy);

        assert_eq!(coordinator.stream_read_timeout(), Duration::from_millis(10));
        assert!(matches!(
            coordinator.on_stream_timeout(),
            TimeoutTransition::NoPendingVoucher
        ));

        coordinator.begin_pending_voucher("auth-1".to_string(), "idem-1".to_string());
        assert_eq!(coordinator.stream_read_timeout(), Duration::from_millis(2));

        match coordinator.on_stream_timeout() {
            TimeoutTransition::Retry(attempt) => {
                assert_eq!(attempt.auth, "auth-1");
                assert_eq!(attempt.idempotency_key, "idem-1");
                assert_eq!(attempt.retry_count, 1);
                assert_eq!(attempt.max_voucher_retries, 2);
            }
            other => panic!("expected retry transition, got {other:?}"),
        }
        assert_eq!(coordinator.stream_read_timeout(), Duration::from_millis(4));

        match coordinator.on_stream_timeout() {
            TimeoutTransition::Retry(attempt) => {
                assert_eq!(attempt.retry_count, 2);
            }
            other => panic!("expected retry transition, got {other:?}"),
        }
        assert_eq!(
            coordinator.stream_read_timeout(),
            Duration::from_millis(8),
            "stall timeout should continue exponential backoff"
        );

        assert!(matches!(
            coordinator.on_stream_timeout(),
            TimeoutTransition::RetryExhausted {
                max_voucher_retries: 2
            }
        ));

        coordinator.clear_pending_voucher();
        assert_eq!(coordinator.stream_read_timeout(), Duration::from_millis(10));
    }

    #[test]
    fn voucher_retry_coordinator_caps_stall_timeout_at_normal_timeout() {
        let policy = VoucherRetryPolicy {
            max_voucher_retries: 4,
            normal_timeout: Duration::from_millis(10),
            base_stall_timeout: Duration::from_millis(8),
        };
        let mut coordinator = VoucherRetryCoordinator::new(policy);
        coordinator.begin_pending_voucher("auth-1".to_string(), "idem-1".to_string());

        assert!(matches!(
            coordinator.on_stream_timeout(),
            TimeoutTransition::Retry(_)
        ));
        assert_eq!(coordinator.stream_read_timeout(), Duration::from_millis(10));

        assert!(matches!(
            coordinator.on_stream_timeout(),
            TimeoutTransition::Retry(_)
        ));
        assert_eq!(coordinator.stream_read_timeout(), Duration::from_millis(10));
    }

    #[test]
    fn test_parse_sse_chunk_openai_delta_content() {
        let raw = r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let (content, finished) = parse_sse_chunk(raw);
        assert_eq!(content.as_deref(), Some("Hello"));
        assert!(!finished);
    }

    #[test]
    fn test_parse_sse_chunk_finish_reason_stop() {
        let raw = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        let (content, finished) = parse_sse_chunk(raw);
        assert!(content.is_none());
        assert!(finished);
    }

    #[test]
    fn test_parse_sse_chunk_role_only_delta() {
        let raw = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        let (content, finished) = parse_sse_chunk(raw);
        assert!(content.is_none());
        assert!(!finished);
    }

    #[test]
    fn test_parse_sse_chunk_empty_content() {
        let raw = r#"{"choices":[{"delta":{"content":""},"finish_reason":null}]}"#;
        let (content, finished) = parse_sse_chunk(raw);
        assert!(content.is_none());
        assert!(!finished);
    }

    #[test]
    fn test_parse_sse_chunk_plain_text() {
        let raw = "some plain text response";
        let (content, finished) = parse_sse_chunk(raw);
        assert_eq!(content.as_deref(), Some("some plain text response"));
        assert!(!finished);
    }

    #[test]
    fn test_parse_sse_chunk_whitespace_trimmed() {
        let raw = "  hello world  \n";
        let (content, finished) = parse_sse_chunk(raw);
        assert_eq!(content.as_deref(), Some("hello world"));
        assert!(!finished);
    }

    #[test]
    fn test_parse_sse_chunk_json_no_choices() {
        let raw = r#"{"model":"gpt-4"}"#;
        let (content, finished) = parse_sse_chunk(raw);
        assert!(content.is_none());
        assert!(!finished);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_voucher_update_falls_back_to_post_when_head_not_supported() {
        let _test_guard = lock_head_cache_tests().await;
        clear_head_unsupported_cache();
        let head_calls = Arc::new(AtomicUsize::new(0));
        let post_calls = Arc::new(AtomicUsize::new(0));
        let channel_id = alloy::primitives::B256::from([0x44; 32]);
        let receipt_header = encode_session_receipt_header(channel_id, 12, 7);

        let head_calls_clone = Arc::clone(&head_calls);
        let post_calls_clone = Arc::clone(&post_calls);
        let app = Router::new().route(
            "/voucher",
            head(move || {
                let head_calls = Arc::clone(&head_calls_clone);
                async move {
                    head_calls.fetch_add(1, Ordering::Relaxed);
                    StatusCode::METHOD_NOT_ALLOWED
                }
            })
            .post(move || {
                let post_calls = Arc::clone(&post_calls_clone);
                let receipt_header = receipt_header.clone();
                async move {
                    post_calls.fetch_add(1, Ordering::Relaxed);
                    (StatusCode::OK, [("payment-receipt", receipt_header)])
                }
            }),
        );

        let (url, server) = spawn_test_server(app).await;

        let signer = test_signer();
        let state = test_state(channel_id);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let result = submit_voucher_update(VoucherSubmitContext {
            url: &url,
            signer: &signer,
            did: "did:pkh:eip155:4217:0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
            debug_enabled: false,
            client: &client,
            state: &state,
            auth: "Payment test",
            idempotency_key: "idem-123",
        })
        .await;

        server.abort();
        let _ = server.await;

        assert!(result.is_ok());
        assert_eq!(head_calls.load(Ordering::Relaxed), 1);
        assert_eq!(post_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_voucher_update_head_success_does_not_fallback_to_post() {
        let _test_guard = lock_head_cache_tests().await;
        clear_head_unsupported_cache();
        let head_calls = Arc::new(AtomicUsize::new(0));
        let post_calls = Arc::new(AtomicUsize::new(0));
        let channel_id = alloy::primitives::B256::from([0x45; 32]);
        let receipt_header = encode_session_receipt_header(channel_id, 12, 7);

        let head_calls_clone = Arc::clone(&head_calls);
        let post_calls_clone = Arc::clone(&post_calls);
        let app = Router::new().route(
            "/voucher",
            head(move || {
                let head_calls = Arc::clone(&head_calls_clone);
                let receipt_header = receipt_header.clone();
                async move {
                    head_calls.fetch_add(1, Ordering::Relaxed);
                    (StatusCode::OK, [("payment-receipt", receipt_header)])
                }
            })
            .post(move || {
                let post_calls = Arc::clone(&post_calls_clone);
                async move {
                    post_calls.fetch_add(1, Ordering::Relaxed);
                    StatusCode::OK
                }
            }),
        );

        let (url, server) = spawn_test_server(app).await;

        let signer = test_signer();
        let state = test_state(channel_id);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let result = submit_voucher_update(VoucherSubmitContext {
            url: &url,
            signer: &signer,
            did: "did:pkh:eip155:4217:0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
            debug_enabled: false,
            client: &client,
            state: &state,
            auth: "Payment test",
            idempotency_key: "idem-456",
        })
        .await;

        server.abort();
        let _ = server.await;

        assert!(result.is_ok());
        assert_eq!(head_calls.load(Ordering::Relaxed), 1);
        assert_eq!(post_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_voucher_update_caches_head_unsupported_per_origin() {
        let _test_guard = lock_head_cache_tests().await;
        clear_head_unsupported_cache();
        let head_calls = Arc::new(AtomicUsize::new(0));
        let post_calls = Arc::new(AtomicUsize::new(0));
        let channel_id = alloy::primitives::B256::from([0x46; 32]);
        let receipt_header = encode_session_receipt_header(channel_id, 12, 7);

        let head_calls_clone = Arc::clone(&head_calls);
        let post_calls_clone = Arc::clone(&post_calls);
        let app = Router::new().route(
            "/voucher",
            head(move || {
                let head_calls = Arc::clone(&head_calls_clone);
                async move {
                    head_calls.fetch_add(1, Ordering::Relaxed);
                    StatusCode::NOT_FOUND
                }
            })
            .post(move || {
                let post_calls = Arc::clone(&post_calls_clone);
                let receipt_header = receipt_header.clone();
                async move {
                    post_calls.fetch_add(1, Ordering::Relaxed);
                    (StatusCode::OK, [("payment-receipt", receipt_header)])
                }
            }),
        );

        let (url, server) = spawn_test_server(app).await;

        let signer = test_signer();
        let state = test_state(channel_id);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let first = submit_voucher_update(VoucherSubmitContext {
            url: &url,
            signer: &signer,
            did: "did:pkh:eip155:4217:0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
            debug_enabled: false,
            client: &client,
            state: &state,
            auth: "Payment test",
            idempotency_key: "idem-cache-1",
        })
        .await;

        let second = submit_voucher_update(VoucherSubmitContext {
            url: &url,
            signer: &signer,
            did: "did:pkh:eip155:4217:0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
            debug_enabled: false,
            client: &client,
            state: &state,
            auth: "Payment test",
            idempotency_key: "idem-cache-2",
        })
        .await;

        server.abort();
        let _ = server.await;

        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(head_calls.load(Ordering::Relaxed), 1);
        assert_eq!(post_calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_voucher_treats_background_5xx_as_transient() {
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_clone = Arc::clone(&head_calls);
        let app = Router::new().route(
            "/voucher",
            head(move || {
                let head_calls = Arc::clone(&head_calls_clone);
                async move {
                    head_calls.fetch_add(1, Ordering::Relaxed);
                    (StatusCode::INTERNAL_SERVER_ERROR, "voucher failed")
                }
            })
            .post(|| async move { StatusCode::OK }),
        );

        let (url, server) = spawn_test_server(app).await;

        let signer = test_signer();
        let echo = test_echo();
        let did = format!("did:pkh:eip155:4217:{:#x}", signer.from);
        let http = test_http_client();
        let reqwest_client = reqwest::Client::builder().no_proxy().build().unwrap();

        let ctx = ChannelContext {
            signer: &signer,
            payer: signer.from,
            echo: &echo,
            did: &did,
            http: &http,
            url: &url,
            rpc_url: "http://127.0.0.1:8545",
            network_id: tempo_common::network::NetworkId::Tempo,
            origin: "http://127.0.0.1",
            top_up_deposit: 100,
            clamped_deposit: None,
            fee_payer: false,
            salt: "0x00".to_string(),
            payee: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            reqwest_client: &reqwest_client,
        };

        let state = test_state(alloy::primitives::B256::from([0x55; 32]));
        let voucher_client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (result_tx, mut result_rx) = mpsc::unbounded_channel::<VoucherTaskResult>();

        post_voucher(
            &ctx,
            &voucher_client,
            "Payment test",
            "idem-background",
            &state,
            result_tx,
        );

        let task_result = tokio::time::timeout(Duration::from_secs(2), result_rx.recv())
            .await
            .expect("background task timed out")
            .expect("background task result missing");

        server.abort();
        let _ = server.await;

        assert_eq!(head_calls.load(Ordering::Relaxed), 1);
        assert!(task_result.result.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_sse_response_allows_missing_receipt_when_stream_completes() {
        let app = Router::new().route(
            "/stream",
            get(|| async {
                (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n",
                )
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/stream");

        let signer = test_signer();
        let echo = test_echo();
        let did = format!("did:pkh:eip155:4217:{:#x}", signer.from);
        let http = test_http_client();
        let reqwest_client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut state = test_state(alloy::primitives::B256::from([0x69; 32]));

        let ctx = ChannelContext {
            signer: &signer,
            payer: signer.from,
            echo: &echo,
            did: &did,
            http: &http,
            url: &url,
            rpc_url: "http://127.0.0.1:8545",
            network_id: tempo_common::network::NetworkId::Tempo,
            origin: "http://127.0.0.1",
            top_up_deposit: 100,
            clamped_deposit: None,
            fee_payer: false,
            salt: "0x00".to_string(),
            payee: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            reqwest_client: &reqwest_client,
        };

        let response = reqwest_client.get(&url).send().await.unwrap();
        let result = stream_sse_response(&ctx, &mut state, response).await;

        server.abort();
        let _ = server.await;

        assert!(
            result.is_ok(),
            "completed streams without payment receipt should remain compatible: {:#}",
            result.unwrap_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_sse_response_requires_valid_receipt_event_for_strict_sessions() {
        let expected_channel = alloy::primitives::B256::from([0x6A; 32]);
        let wrong_channel = alloy::primitives::B256::from([0x6B; 32]);
        let receipt_event = format!(
            "event: payment-receipt\ndata: {{\"method\":\"tempo\",\"intent\":\"session\",\"status\":\"success\",\"timestamp\":\"2026-03-15T00:00:01Z\",\"reference\":\"0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\",\"challengeId\":\"session-it\",\"channelId\":\"{wrong_channel:#x}\",\"acceptedCumulative\":\"12\",\"spent\":\"7\",\"units\":1,\"txHash\":\"0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\"}}\n\n"
        );

        let app = Router::new().route(
            "/stream",
            get({
                let receipt_event = receipt_event.clone();
                move || {
                    let receipt_event = receipt_event.clone();
                    async move {
                        (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            Body::from_stream(futures::stream::once(async {
                                Ok::<Bytes, std::io::Error>(Bytes::from(receipt_event))
                            })),
                        )
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/stream");

        let signer = test_signer();
        let echo = test_echo();
        let did = format!("did:pkh:eip155:4217:{:#x}", signer.from);
        let http = test_http_client();
        let reqwest_client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut state = test_state(expected_channel);

        let ctx = ChannelContext {
            signer: &signer,
            payer: signer.from,
            echo: &echo,
            did: &did,
            http: &http,
            url: &url,
            rpc_url: "http://127.0.0.1:8545",
            network_id: tempo_common::network::NetworkId::Tempo,
            origin: "http://127.0.0.1",
            top_up_deposit: 100,
            clamped_deposit: None,
            fee_payer: false,
            salt: "0x00".to_string(),
            payee: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            reqwest_client: &reqwest_client,
        };

        let response = reqwest_client.get(&url).send().await.unwrap();
        let result = stream_sse_response(&ctx, &mut state, response).await;

        server.abort();
        let _ = server.await;

        let err = result.expect_err("strict sessions should reject invalid receipt events");
        assert!(err.to_string().contains("Invalid required Payment-Receipt"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_sse_response_header_receipt_updates_server_spent() {
        let channel_id = alloy::primitives::B256::from([0x68; 32]);
        let receipt_header = encode_session_receipt_header(channel_id, 12, 7);

        let app = Router::new().route(
            "/stream",
            get({
                let receipt_header = receipt_header.clone();
                move || {
                    let receipt_header = receipt_header.clone();
                    async move {
                        axum::http::Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .header("payment-receipt", receipt_header)
                            .body(Body::from(
                                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}] }\n\n",
                            ))
                            .unwrap()
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/stream");

        let signer = test_signer();
        let echo = test_echo();
        let did = format!("did:pkh:eip155:4217:{:#x}", signer.from);
        let http = test_http_client();
        let reqwest_client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut state = test_state(channel_id);

        let ctx = ChannelContext {
            signer: &signer,
            payer: signer.from,
            echo: &echo,
            did: &did,
            http: &http,
            url: &url,
            rpc_url: "http://127.0.0.1:8545",
            network_id: tempo_common::network::NetworkId::Tempo,
            origin: "http://127.0.0.1",
            top_up_deposit: 100,
            clamped_deposit: None,
            fee_payer: false,
            salt: "0x00".to_string(),
            payee: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            reqwest_client: &reqwest_client,
        };

        let response = reqwest_client.get(&url).send().await.unwrap();
        let result = stream_sse_response(&ctx, &mut state, response).await;

        server.abort();
        let _ = server.await;

        assert!(result.is_ok());
        assert_eq!(state.accepted_cumulative, 12);
        assert_eq!(state.cumulative_amount, 12);
        assert_eq!(state.server_spent, 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_sse_response_header_receipt_allows_reconciled_lower_spent() {
        let channel_id = alloy::primitives::B256::from([0x70; 32]);
        let receipt_header = encode_session_receipt_header(channel_id, 12, 7);

        let app = Router::new().route(
            "/stream",
            get({
                let receipt_header = receipt_header.clone();
                move || {
                    let receipt_header = receipt_header.clone();
                    async move {
                        axum::http::Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .header("payment-receipt", receipt_header)
                            .body(Body::from(
                                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}] }\n\n",
                            ))
                            .unwrap()
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/stream");

        let signer = test_signer();
        let echo = test_echo();
        let did = format!("did:pkh:eip155:4217:{:#x}", signer.from);
        let http = test_http_client();
        let reqwest_client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut state = test_state(channel_id);
        state.server_spent = 10;

        let ctx = ChannelContext {
            signer: &signer,
            payer: signer.from,
            echo: &echo,
            did: &did,
            http: &http,
            url: &url,
            rpc_url: "http://127.0.0.1:8545",
            network_id: tempo_common::network::NetworkId::Tempo,
            origin: "http://127.0.0.1",
            top_up_deposit: 100,
            clamped_deposit: None,
            fee_payer: false,
            salt: "0x00".to_string(),
            payee: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            reqwest_client: &reqwest_client,
        };

        let response = reqwest_client.get(&url).send().await.unwrap();
        let result = stream_sse_response(&ctx, &mut state, response).await;

        server.abort();
        let _ = server.await;

        assert!(result.is_ok());
        assert_eq!(state.server_spent, 7);
    }

    #[serial_test::serial]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_sse_response_idle_timeout_returns_error() {
        std::env::set_var("TEMPO_SESSION_NORMAL_TIMEOUT_MS", "10");
        std::env::remove_var("TEMPO_SESSION_MAX_VOUCHER_RETRIES");
        std::env::remove_var("TEMPO_SESSION_STALL_TIMEOUT_MS");

        let app = Router::new().route(
            "/stream",
            get(|| async {
                (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>()),
                )
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/stream");

        let signer = test_signer();
        let echo = test_echo();
        let did = format!("did:pkh:eip155:4217:{:#x}", signer.from);
        let http = test_http_client();
        let reqwest_client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut state = test_state(alloy::primitives::B256::from([0x68; 32]));

        let ctx = ChannelContext {
            signer: &signer,
            payer: signer.from,
            echo: &echo,
            did: &did,
            http: &http,
            url: &url,
            rpc_url: "http://127.0.0.1:8545",
            network_id: tempo_common::network::NetworkId::Tempo,
            origin: "http://127.0.0.1",
            top_up_deposit: 100,
            clamped_deposit: None,
            fee_payer: false,
            salt: "0x00".to_string(),
            payee: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            reqwest_client: &reqwest_client,
        };

        let response = reqwest_client.get(&url).send().await.unwrap();
        let result = stream_sse_response(&ctx, &mut state, response).await;

        server.abort();
        let _ = server.await;
        std::env::remove_var("TEMPO_SESSION_NORMAL_TIMEOUT_MS");

        let err = result.expect_err("idle timeout should error");
        assert!(
            matches!(
                err,
                TempoError::Payment(PaymentError::SessionStreamIdleTimeout { .. })
            ),
            "unexpected error: {err:#}"
        );
    }

    #[serial_test::serial]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_sse_response_voucher_retry_exhaustion_returns_error() {
        std::env::set_var("TEMPO_SESSION_NORMAL_TIMEOUT_MS", "10");
        std::env::set_var("TEMPO_SESSION_STALL_TIMEOUT_MS", "5");
        std::env::set_var("TEMPO_SESSION_MAX_VOUCHER_RETRIES", "0");

        let channel_id = alloy::primitives::B256::from([0x69; 32]);
        let receipt_header = encode_session_receipt_header(channel_id, 12, 7);
        let need_voucher = format!(
            "event: payment-need-voucher\ndata: {{\"channelId\":\"{channel_id:#x}\",\"requiredCumulative\":\"12\",\"acceptedCumulative\":\"10\",\"deposit\":\"100\"}}\n\n"
        );

        let app = Router::new()
            .route(
                "/stream",
                get({
                    let need_voucher = need_voucher.clone();
                    move || {
                        let need_voucher = need_voucher.clone();
                        async move {
                            let body_stream = futures::stream::once(async {
                                Ok::<Bytes, std::io::Error>(Bytes::from(need_voucher))
                            })
                            .chain(futures::stream::pending::<Result<Bytes, std::io::Error>>());
                            (
                                StatusCode::OK,
                                [("content-type", "text/event-stream")],
                                Body::from_stream(body_stream),
                            )
                        }
                    }
                }),
            )
            .route(
                "/stream",
                head({
                    let receipt_header = receipt_header.clone();
                    move || {
                        let receipt_header = receipt_header.clone();
                        async move { (StatusCode::OK, [("payment-receipt", receipt_header)]) }
                    }
                }),
            )
            .route(
                "/stream",
                post({
                    let receipt_header = receipt_header.clone();
                    move || {
                        let receipt_header = receipt_header.clone();
                        async move { (StatusCode::OK, [("payment-receipt", receipt_header)]) }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/stream");

        let signer = test_signer();
        let echo = test_echo();
        let did = format!("did:pkh:eip155:4217:{:#x}", signer.from);
        let http = test_http_client();
        let reqwest_client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut state = test_state(channel_id);

        let ctx = ChannelContext {
            signer: &signer,
            payer: signer.from,
            echo: &echo,
            did: &did,
            http: &http,
            url: &url,
            rpc_url: "http://127.0.0.1:8545",
            network_id: tempo_common::network::NetworkId::Tempo,
            origin: "http://127.0.0.1",
            top_up_deposit: 100,
            clamped_deposit: None,
            fee_payer: false,
            salt: "0x00".to_string(),
            payee: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            reqwest_client: &reqwest_client,
        };

        let response = reqwest_client.get(&url).send().await.unwrap();
        let result = stream_sse_response(&ctx, &mut state, response).await;

        server.abort();
        let _ = server.await;
        std::env::remove_var("TEMPO_SESSION_NORMAL_TIMEOUT_MS");
        std::env::remove_var("TEMPO_SESSION_STALL_TIMEOUT_MS");
        std::env::remove_var("TEMPO_SESSION_MAX_VOUCHER_RETRIES");

        let err = result.expect_err("retry exhaustion should error");
        assert!(
            matches!(
                err,
                TempoError::Payment(PaymentError::SessionVoucherRetryExhausted { .. })
            ),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_sse_response_rejects_deposit_below_local_cumulative_floor() {
        let channel_id = alloy::primitives::B256::from([0x70; 32]);
        let need_voucher = format!(
            "event: payment-need-voucher\ndata: {{\"channelId\":\"{channel_id:#x}\",\"requiredCumulative\":\"5\",\"acceptedCumulative\":\"5\",\"deposit\":\"5\"}}\n\n"
        );

        let app = Router::new().route(
            "/stream",
            get({
                let need_voucher = need_voucher.clone();
                move || {
                    let need_voucher = need_voucher.clone();
                    async move {
                        (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            Body::from_stream(futures::stream::once(async {
                                Ok::<Bytes, std::io::Error>(Bytes::from(need_voucher))
                            })),
                        )
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/stream");

        let signer = test_signer();
        let echo = test_echo();
        let did = format!("did:pkh:eip155:4217:{:#x}", signer.from);
        let http = test_http_client();
        let reqwest_client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut state = test_state(channel_id);
        assert_eq!(state.cumulative_amount, 10, "test precondition");

        let ctx = ChannelContext {
            signer: &signer,
            payer: signer.from,
            echo: &echo,
            did: &did,
            http: &http,
            url: &url,
            rpc_url: "http://127.0.0.1:8545",
            network_id: tempo_common::network::NetworkId::Tempo,
            origin: "http://127.0.0.1",
            top_up_deposit: 100,
            clamped_deposit: None,
            fee_payer: false,
            salt: "0x00".to_string(),
            payee: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            reqwest_client: &reqwest_client,
        };

        let response = reqwest_client.get(&url).send().await.unwrap();
        let result = stream_sse_response(&ctx, &mut state, response).await;

        server.abort();
        let _ = server.await;

        let err = result.expect_err("deposit below local floor should fail closed");
        assert!(
            err.to_string()
                .contains("deposit below local cumulative floor"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn drain_voucher_results_surfaces_task_errors() {
        let (result_tx, mut result_rx) = mpsc::unbounded_channel::<VoucherTaskResult>();
        let mut in_flight_voucher_tasks = 1;
        let send_result = result_tx.send(VoucherTaskResult {
            _idempotency_key: "idem-err".to_string(),
            result: Err(PaymentError::PaymentRejected {
                reason: "voucher submit failed".to_string(),
                status_code: 500,
            }
            .into()),
        });
        assert!(send_result.is_ok());

        let err = drain_voucher_results(&mut result_rx, &mut in_flight_voucher_tasks)
            .expect_err("should return task error");
        assert!(err.to_string().contains("voucher submit failed"));
        assert_eq!(in_flight_voucher_tasks, 0);
    }

    #[serial_test::serial]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_sse_response_succeeds_even_when_pending_voucher_task_never_completes() {
        std::env::set_var("TEMPO_SESSION_NORMAL_TIMEOUT_MS", "10");
        std::env::remove_var("TEMPO_SESSION_STALL_TIMEOUT_MS");
        std::env::remove_var("TEMPO_SESSION_MAX_VOUCHER_RETRIES");

        let channel_id = alloy::primitives::B256::from([0x71; 32]);
        let receipt_header = encode_session_receipt_header(channel_id, 12, 7);
        let need_voucher = format!(
            "event: payment-need-voucher\ndata: {{\"channelId\":\"{channel_id:#x}\",\"requiredCumulative\":\"12\",\"acceptedCumulative\":\"10\",\"deposit\":\"100\"}}\n\n"
        );
        let done = "data: [DONE]\n\n".to_string();

        let app = Router::new().route(
            "/stream",
            get({
                let need_voucher = need_voucher.clone();
                let done = done.clone();
                let receipt_header = receipt_header.clone();
                move || {
                    let need_voucher = need_voucher.clone();
                    let done = done.clone();
                    let receipt_header = receipt_header.clone();
                    async move {
                        let body_stream = futures::stream::iter(vec![
                            Ok::<Bytes, std::io::Error>(Bytes::from(need_voucher)),
                            Ok::<Bytes, std::io::Error>(Bytes::from(done)),
                        ]);
                        axum::http::Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .header("payment-receipt", receipt_header)
                            .body(Body::from_stream(body_stream))
                            .unwrap()
                    }
                }
            })
            .head(|| async move { std::future::pending::<StatusCode>().await }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/stream");

        let signer = test_signer();
        let echo = test_echo();
        let did = format!("did:pkh:eip155:4217:{:#x}", signer.from);
        let http = test_http_client();
        let reqwest_client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut state = test_state(channel_id);

        let ctx = ChannelContext {
            signer: &signer,
            payer: signer.from,
            echo: &echo,
            did: &did,
            http: &http,
            url: &url,
            rpc_url: "http://127.0.0.1:8545",
            network_id: tempo_common::network::NetworkId::Tempo,
            origin: "http://127.0.0.1",
            top_up_deposit: 100,
            clamped_deposit: None,
            fee_payer: false,
            salt: "0x00".to_string(),
            payee: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            reqwest_client: &reqwest_client,
        };

        let response = reqwest_client.get(&url).send().await.unwrap();
        let result = stream_sse_response(&ctx, &mut state, response).await;

        server.abort();
        let _ = server.await;
        std::env::remove_var("TEMPO_SESSION_NORMAL_TIMEOUT_MS");

        // When the stream completes normally ([DONE]), stale voucher tasks
        // are not awaited — blocking on them would delay exit unnecessarily.
        assert!(
            result.is_ok(),
            "stream should succeed despite pending voucher task: {:#}",
            result.unwrap_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_sse_response_tolerates_transient_voucher_500() {
        let channel_id = alloy::primitives::B256::from([0x72; 32]);
        let receipt_header = encode_session_receipt_header(channel_id, 12, 7);
        let need_voucher = format!(
            "event: payment-need-voucher\ndata: {{\"channelId\":\"{channel_id:#x}\",\"requiredCumulative\":\"12\",\"acceptedCumulative\":\"10\",\"deposit\":\"100\"}}\n\n"
        );
        let done = "data: [DONE]\n\n".to_string();

        let app = Router::new().route(
            "/stream",
            get({
                let need_voucher = need_voucher.clone();
                let done = done.clone();
                let receipt_header = receipt_header.clone();
                move || {
                    let need_voucher = need_voucher.clone();
                    let done = done.clone();
                    let receipt_header = receipt_header.clone();
                    async move {
                        let body_stream = futures::stream::iter(vec![
                            Ok::<Bytes, std::io::Error>(Bytes::from(need_voucher)),
                            Ok::<Bytes, std::io::Error>(Bytes::from(done)),
                        ]);
                        axum::http::Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .header("payment-receipt", receipt_header)
                            .body(Body::from_stream(body_stream))
                            .unwrap()
                    }
                }
            })
            .head(|| async { StatusCode::NOT_FOUND })
            .post(|| async {
                axum::http::Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from(""))
                    .unwrap()
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/stream");

        let signer = test_signer();
        let echo = test_echo();
        let did = format!("did:pkh:eip155:4217:{:#x}", signer.from);
        let http = test_http_client();
        let reqwest_client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut state = test_state(channel_id);

        let ctx = ChannelContext {
            signer: &signer,
            payer: signer.from,
            echo: &echo,
            did: &did,
            http: &http,
            url: &url,
            rpc_url: "http://127.0.0.1:8545",
            network_id: tempo_common::network::NetworkId::Tempo,
            origin: "http://127.0.0.1",
            top_up_deposit: 100,
            clamped_deposit: None,
            fee_payer: false,
            salt: "0x00".to_string(),
            payee: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            reqwest_client: &reqwest_client,
        };

        let response = reqwest_client.get(&url).send().await.unwrap();
        let result = stream_sse_response(&ctx, &mut state, response).await;

        server.abort();
        let _ = server.await;

        assert!(
            result.is_ok(),
            "stream should succeed when voucher update receives transient 500: {:#}",
            result.unwrap_err()
        );
    }
}
