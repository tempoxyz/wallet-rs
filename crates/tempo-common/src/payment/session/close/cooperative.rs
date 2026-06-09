//! Cooperative (server-side) channel close.
//!
//! Sends a close credential to the server and lets it settle the channel
//! on-chain, avoiding the payer-initiated grace period.

use alloy::primitives::{Address, B256};

use mpp::{
    parse_receipt,
    protocol::methods::tempo::{session::SessionCredentialPayload, sign_voucher},
    ChallengeEcho,
};

use mpp::protocol::core::extract_tx_hash;

use super::super::store;
use crate::{
    cli::{format::format_token_amount, terminal::sanitize_for_terminal},
    error::{KeyError, NetworkError, PaymentError, TempoError},
    payment::classify::parse_problem_details,
};

type ChannelResult<T> = Result<T, TempoError>;

fn credential_source_from_payer(payer: &str, chain_id: u64) -> String {
    if payer.starts_with("did:pkh:eip155:") {
        return payer.to_string();
    }

    if let Ok(address) = payer.parse::<Address>() {
        return format!("did:pkh:eip155:{chain_id}:{address:#x}");
    }

    format!("did:pkh:eip155:{chain_id}:{}", payer.trim())
}

/// Try cooperative close via the server.
///
/// Returns the settlement transaction URL on success (if available).
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_server_close(
    record: &store::ChannelRecord,
    echo: &ChallengeEcho,
    signer: &alloy::signers::local::PrivateKeySigner,
    channel_id: B256,
    escrow_contract: Address,
    chain_id: u64,
    cumulative_amount: u128,
    client: &reqwest::Client,
) -> ChannelResult<Option<String>> {
    let close_url = if record.request_url.is_empty() {
        &record.origin
    } else {
        &record.request_url
    };

    let fresh_echo = match client.post(close_url).send().await {
        Ok(resp) if resp.status().as_u16() == 402 => resp
            .headers()
            .get_all("www-authenticate")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|wa| mpp::parse_www_authenticate(wa).ok())
            .find(|ch| ch.intent.is_session())
            .map(|ch| ch.to_echo()),
        _ => None,
    };
    let echo = fresh_echo.as_ref().unwrap_or(echo);

    let network_id = record.network_id();
    let spent_fmt = format_token_amount(cumulative_amount, network_id);
    let deposit_u = record.deposit;
    let deposit_fmt = format_token_amount(deposit_u, network_id);
    tracing::info!(
        spent = %spent_fmt,
        deposit = %deposit_fmt,
        channel = %format_args!("{:#x}", channel_id),
        url = close_url,
        "coop close"
    );
    let payload = if record.session_protocol_or_legacy()
        == mpp::protocol::methods::tempo::session::SESSION_PROTOCOL_TIP1034
    {
        let descriptor = record
            .descriptor()
            .ok_or_else(|| KeyError::SigningOperation {
                operation: "sign TIP-1034 close voucher",
                reason: "missing channel descriptor".to_string(),
            })?;
        mpp::client::tempo::session::channel_ops::create_precompile_close_payload_with_descriptor_and_escrow(
            signer,
            channel_id,
            descriptor,
            cumulative_amount,
            escrow_contract,
            chain_id,
        )
        .await
        .map_err(|source| KeyError::SigningOperationSource {
            operation: "sign TIP-1034 close voucher",
            source: Box::new(source),
        })?
    } else {
        let sig = sign_voucher(
            signer,
            channel_id,
            cumulative_amount,
            escrow_contract,
            chain_id,
        )
        .await
        .map_err(|source| KeyError::SigningOperationSource {
            operation: "sign close voucher",
            source: Box::new(source),
        })?;
        SessionCredentialPayload::Close {
            channel_id: format!("{channel_id:#x}"),
            descriptor: None,
            cumulative_amount: cumulative_amount.to_string(),
            signature: format!("0x{}", hex::encode(sig)),
        }
    };
    let source = credential_source_from_payer(&record.payer, chain_id);
    let credential = mpp::PaymentCredential::with_source(echo.clone(), source, payload);
    let auth = mpp::format_authorization(&credential).map_err(|source| {
        PaymentError::ChallengeFormatSource {
            context: "close credential",
            source: Box::new(source),
        }
    })?;
    let response = client
        .post(close_url)
        .header("Authorization", &auth)
        .send()
        .await
        .map_err(NetworkError::Reqwest)?;

    // Interpret response and optionally retry once with required cumulative
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| String::from("<no body>"));
        let raw_reason: String = if let Some(problem) = parse_problem_details(&body) {
            problem.message()
        } else if body.trim().is_empty() {
            format!("HTTP {}", status.as_u16())
        } else {
            body.chars().take(500).collect()
        };
        let reason = sanitize_for_terminal(&raw_reason);
        return Err(PaymentError::PaymentRejected {
            reason,
            status_code: status.as_u16(),
        }
        .into());
    }

    let tx_url = response
        .headers()
        .get("payment-receipt")
        .and_then(|v| v.to_str().ok())
        .and_then(|receipt_str| {
            let receipt = parse_receipt(receipt_str).ok()?;
            let tx_ref = extract_tx_hash(receipt_str).unwrap_or(receipt.reference);
            Some(record.network_id().tx_url(&tx_ref))
        });

    Ok(tx_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Router};
    use std::sync::{Arc, Mutex};
    use tokio::task::JoinHandle;

    #[test]
    fn credential_source_derives_did_from_raw_address() {
        let source =
            credential_source_from_payer("0x0000000000000000000000000000000000000003", 4217);
        assert_eq!(
            source,
            "did:pkh:eip155:4217:0x0000000000000000000000000000000000000003"
        );
    }

    #[test]
    fn credential_source_preserves_existing_did() {
        let source = credential_source_from_payer(
            "did:pkh:eip155:4217:0x0000000000000000000000000000000000000003",
            4217,
        );
        assert_eq!(
            source,
            "did:pkh:eip155:4217:0x0000000000000000000000000000000000000003"
        );
    }

    #[test]
    fn close_payload_uses_spec_field_names() {
        let payload = SessionCredentialPayload::Close {
            channel_id: "0xabc".to_string(),
            descriptor: None,
            cumulative_amount: "42".to_string(),
            signature: "0xdeadbeef".to_string(),
        };
        let value = serde_json::to_value(payload).unwrap();
        assert_eq!(value["action"], "close");
        assert_eq!(value["channelId"], "0xabc");
        assert_eq!(value["cumulativeAmount"], "42");
        assert_eq!(value["signature"], "0xdeadbeef");
    }

    async fn spawn_test_server() -> (String, Arc<Mutex<(usize, usize)>>, JoinHandle<()>) {
        let counters = Arc::new(Mutex::new((0usize, 0usize)));
        let counters_clone = counters.clone();
        let app = Router::new().route(
            "/",
            post(move |req: axum::http::Request<axum::body::Body>| {
                let counters = counters_clone.clone();
                async move {
                    let has_auth = req.headers().get("authorization").is_some();
                    let mut c = counters.lock().unwrap();
                    if has_auth {
                        c.1 += 1;
                        drop(c);
                        axum::http::Response::builder()
                            .status(200)
                            .body(axum::body::Body::empty())
                            .unwrap()
                    } else {
                        c.0 += 1;
                        drop(c);
                        axum::http::Response::builder()
                            .status(402)
                            .header(
                                "www-authenticate",
                                r#"Payment id="abc", realm="test", method="tempo", intent="session", request="e30""#,
                            )
                            .body(axum::body::Body::empty())
                            .unwrap()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            format!("http://{}:{}/", addr.ip(), addr.port()),
            counters,
            handle,
        )
    }

    #[tokio::test]
    async fn test_try_server_close_prefetch_and_single_attempt() {
        let (base, counters, _handle) = spawn_test_server().await;

        // Minimal synthetic record
        let record = store::ChannelRecord {
            version: 1,
            origin: base.clone(),
            request_url: base.clone(),
            chain_id: 4217,
            escrow_contract: "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000001".into(),
            payee: "0x0000000000000000000000000000000000000002".into(),
            payer: "0x0000000000000000000000000000000000000003".into(),
            authorized_signer: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            salt: "0x00".into(),
            channel_id: "0x0000000000000000000000000000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            session_protocol: mpp::protocol::methods::tempo::session::SESSION_PROTOCOL_LEGACY
                .to_string(),
            descriptor_json: None,
            deposit: 1000,
            cumulative_amount: 2,
            accepted_cumulative: 0,
            server_spent: 0,
            challenge_echo: serde_json::to_string(&mpp::ChallengeEcho {
                id: "abc".into(),
                realm: "test".into(),
                method: mpp::protocol::core::MethodName::from("tempo"),
                intent: mpp::protocol::core::IntentName::from("session"),
                request: mpp::Base64UrlJson::from_raw("e30"), // base64url of {}
                expires: None,
                digest: None,
                opaque: None,
            })
            .unwrap(),
            state: store::ChannelStatus::Active,
            close_requested_at: 0,
            grace_ready_at: 0,
            created_at: 0,
            last_used_at: 0,
        };

        let echo: ChallengeEcho = serde_json::from_str(&record.challenge_echo).unwrap();
        let signer = alloy::signers::local::PrivateKeySigner::from_bytes(
            &"0x0707070707070707070707070707070707070707070707070707070707070707"
                .parse()
                .unwrap(),
        )
        .unwrap();
        let channel_id: B256 = "0x0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20"
            .parse()
            .unwrap();
        let escrow: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let _ = try_server_close(
            &record, &echo, &signer, channel_id, escrow, 4217, 2, &client,
        )
        .await;

        let (prefetch, authorized) = *counters.lock().unwrap();
        assert_eq!(prefetch, 1, "should prefetch fresh echo with 402");
        assert_eq!(
            authorized, 1,
            "should send exactly one authorized close request"
        );
    }
}
