//! Session-open transaction building and retry logic.
//!
//! Constructs the session-open transaction and retries submission
//! when the server hasn't indexed the channel yet. Low-level signing
//! and broadcast helpers remain in `tempo_common::session::tx`.

use alloy::primitives::{Address, B256};

use super::error_map::payment_rejected_from_body;
use crate::http::HttpClient;
use tempo_common::{
    error::{ConfigError, PaymentError, TempoError},
    keys::Signer,
    payment::{
        classify::{parse_problem_details, SessionProblemType},
        session as common_tx,
    },
};

fn should_retry_open_response(status_code: u16, body: &str) -> bool {
    if status_code != 410 {
        return false;
    }

    parse_problem_details(body)
        .is_some_and(|problem| problem.classify() == SessionProblemType::ChannelNotFound)
}

/// Result of building a Tempo payment from calls.
pub(super) struct TempoPaymentResult {
    pub(super) tx_bytes: Vec<u8>,
    pub(super) expiring_nonce_hash: B256,
}

/// Create a Tempo payment credential from pre-built calls.
///
/// Used by session payments where the calls (e.g., approve + escrow.open)
/// are built externally. Uses expiring nonces and parallelizes fee
/// resolution with gas estimation.
///
/// Returns both the credential (for sending to the server) and the raw
/// signed transaction bytes (for optional client-side pre-broadcast).
pub(super) async fn create_tempo_payment_from_calls(
    rpc_url_str: &str,
    signing: &Signer,
    calls: Vec<tempo_primitives::transaction::Call>,
    fee_token: Address,
    chain_id: u64,
    fee_payer: bool,
) -> Result<TempoPaymentResult, TempoError> {
    let rpc_url: url::Url = rpc_url_str
        .parse()
        .map_err(|source| ConfigError::InvalidUrl {
            context: "RPC",
            source,
        })?;
    let provider = alloy::providers::RootProvider::<mpp::client::TempoNetwork>::new_http(rpc_url);

    let from = signing.from;
    let signed = common_tx::resolve_and_sign_tx_with_fee_payer_info(
        &provider, signing, chain_id, fee_token, from, calls, fee_payer,
    )
    .await?;

    Ok(TempoPaymentResult {
        tx_bytes: signed.tx_bytes,
        expiring_nonce_hash: signed.expiring_nonce_hash,
    })
}

/// Send the Open credential to the server and retry on HTTP 410 while the node indexes.
///
/// Returns the raw `reqwest::Response` on success so that the caller can
/// detect SSE (`text/event-stream`) responses before buffering the body.
pub(super) async fn send_open_with_retry(
    http: &HttpClient,
    url: &str,
    auth_header: &str,
    idempotency_key: &str,
    delays_ms: &[u64],
) -> Result<reqwest::Response, TempoError> {
    let headers = vec![
        ("Authorization".to_string(), auth_header.to_string()),
        ("Idempotency-Key".to_string(), idempotency_key.to_string()),
    ];

    let resp = http.execute_raw_with_retry(url, &headers).await?;

    if resp.status().as_u16() < 400 {
        return Ok(resp);
    }

    if resp.status().as_u16() == 410 {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if should_retry_open_response(status, &body) {
            if http.log_enabled() {
                eprintln!("Server hasn't indexed channel yet, retrying...");
            }
            for delay in delays_ms {
                tokio::time::sleep(std::time::Duration::from_millis(*delay)).await;
                let next = http.execute_raw_with_retry(url, &headers).await?;
                if next.status().as_u16() < 400 {
                    return Ok(next);
                }
                let next_status = next.status().as_u16();
                let next_body = next.text().await.unwrap_or_default();
                if !should_retry_open_response(next_status, &next_body) {
                    return Err(payment_rejected_from_body(next_status, &next_body));
                }
            }
            // Intentional operator-facing retry exhaustion message; this path has
            // no richer source error beyond repeated 410 channel-not-found responses.
            return Err(PaymentError::PaymentRejected {
                reason: "Server could not find channel after retries".to_string(),
                status_code: 410,
            }
            .into());
        }
        return Err(payment_rejected_from_body(410, &body));
    }

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Err(payment_rejected_from_body(status, &body))
}

#[cfg(test)]
mod tests {
    use super::{send_open_with_retry, should_retry_open_response};

    use crate::http::{HttpClient, HttpRequestBody, HttpRequestPlan};

    #[test]
    fn retries_only_for_channel_not_found_problem_type() {
        let body = r#"{"type":"https://paymentauth.org/problems/session/channel-not-found","detail":"channel unknown"}"#;
        assert!(should_retry_open_response(410, body));
    }

    #[test]
    fn does_not_retry_for_non_matching_problem_type() {
        let body = r#"{"type":"https://paymentauth.org/problems/session/signer-mismatch","detail":"bad signer"}"#;
        assert!(!should_retry_open_response(410, body));
    }

    #[test]
    fn does_not_retry_when_status_is_not_410() {
        let body = r#"{"type":"https://paymentauth.org/problems/session/channel-not-found"}"#;
        assert!(!should_retry_open_response(402, body));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_open_replays_original_request_body() {
        let app = axum::Router::new().route(
            "/open",
            axum::routing::post(|headers: axum::http::HeaderMap, body: String| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                if auth != "Payment test" || body != r#"{"model":"priced"}"# {
                    return (axum::http::StatusCode::BAD_REQUEST, "bad open request");
                }
                (axum::http::StatusCode::OK, "ok")
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let plan = HttpRequestPlan {
            method: reqwest::Method::POST,
            body: Some(HttpRequestBody::Bytes(br#"{"model":"priced"}"#.to_vec())),
            ..Default::default()
        };
        let http = HttpClient::new(
            plan,
            tempo_common::cli::Verbosity {
                level: 0,
                show_output: false,
            },
            None,
            false,
            None,
        )
        .unwrap();
        let response = send_open_with_retry(
            &http,
            &format!("http://{addr}/open"),
            "Payment test",
            "idem",
            &[],
        )
        .await
        .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        server.abort();
        let _ = server.await;
    }
}
