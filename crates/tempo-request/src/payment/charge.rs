//! MPP charge payment handling.
//!
//! This module handles the MPP protocol (<https://mpp.dev>) which uses
//! WWW-Authenticate and Authorization headers for HTTP-native payments.
//!
//! For zero-amount charges (`amount = "0"`), the mpp SDK signs an EIP-712
//! proof credential instead of creating a transaction.

use mpp::client::PaymentProvider;

use crate::http::{HttpClient, HttpResponse};
use tempo_common::{
    cli::terminal::sanitize_for_terminal,
    error::{ConfigError, KeyError, PaymentError, TempoError},
    keys::Signer,
};

use super::{
    lock::{acquire_origin_lock, origin_lock_key},
    types::{PaymentResult, ResolvedChallenge},
};
use tempo_common::payment::{classify_payment_error, map_mpp_validation_error};

/// Client identifier for MPP attribution memos, written into the on-chain
/// transaction data so charge payments can be attributed to the Tempo CLI.
const CLIENT_ID: &str = "tempo-wallet";

/// Whether a post-submission response warrants a provisioning retry.
///
/// Only auth/payment codes (401–403) and server errors (5xx) are retried.
/// Other 4xx codes (400 body validation, 404, 422, 429, etc.) are not
/// payment-related and retrying with key_authorization would be wasteful.
fn is_retriable_payment_rejection(response: &HttpResponse) -> bool {
    if matches!(response.status_code, 401..=403 | 500..=599) {
        return true;
    }

    // Some MPP servers wrap Tempo RPC failures behind a generic payment proof
    // error. Treat that specific wrapper as payment-related so first-use access
    // key provisioning can retry with key_authorization attached.
    response
        .body_string()
        .ok()
        .is_some_and(|body| body.to_ascii_lowercase().contains("payment_proof_invalid"))
}

/// Handle an MPP charge payment flow (402 with intent="charge").
///
/// Validates the challenge, builds and signs the transaction,
/// submits the payment, and returns the result.
pub(super) async fn handle_charge_request(
    http: &HttpClient,
    url: &str,
    resolved: ResolvedChallenge,
    signer: Signer,
) -> Result<PaymentResult, TempoError> {
    let challenge = &resolved.challenge;

    challenge
        .validate_for_charge("tempo")
        .map_err(|e| map_mpp_validation_error(e, challenge))?;

    // Detect zero-amount charges — these use EIP-712 proof credentials
    // instead of transactions, so they skip the origin lock and dry-run tx flow.
    let is_zero_amount = challenge
        .request
        .decode::<mpp::ChargeRequest>()
        .ok()
        .is_some_and(|r| r.amount == "0");

    if is_zero_amount {
        return handle_zero_amount_charge(http, url, &resolved, &signer).await;
    }

    if http.dry_run {
        eprintln!("[DRY RUN] Signed transaction ready, skipping submission.");
        return Ok(PaymentResult {
            tx_hash: None,
            channel_id: None,
            status_code: 200,
            response: None,
        });
    }

    // Serialize charge submissions per origin before building/submitting payment
    // credentials to avoid duplicate expiring-nonce tx races under overlap.
    let lock_key = origin_lock_key(url);
    let _charge_lock = acquire_origin_lock(&lock_key)?;

    let provider =
        mpp::client::TempoProvider::new(signer.signer.clone(), resolved.rpc_url.as_str())
            .map_err(|source| ConfigError::ProviderInitSource {
                provider: "tempo payment provider",
                source: Box::new(source),
            })?
            .with_client_id(CLIENT_ID)
            .with_signing_mode(signer.signing_mode.clone());

    let credential = match provider.pay(challenge).await {
        Ok(cred) => cred,
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
            let retry_provider = mpp::client::TempoProvider::new(
                provisioning_signer.signer.clone(),
                resolved.rpc_url.as_str(),
            )
            .map_err(|source| ConfigError::ProviderInitSource {
                provider: "tempo payment provider (provisioning retry)",
                source: Box::new(source),
            })?
            .with_client_id(CLIENT_ID)
            .with_signing_mode(provisioning_signer.signing_mode);
            retry_provider
                .pay(challenge)
                .await
                .map_err(|_| classify_payment_error(original, &resolved.network_id))?
        }
        Err(e) => return Err(classify_payment_error(e, &resolved.network_id)),
    };

    let resp = submit_credential(http, url, &credential).await?;

    // If the server rejects the payment and we have a stored key authorization,
    // retry with provisioning. The server validates the transaction on-chain, and
    // it may fail when the access key isn't provisioned even though signing
    // succeeded locally (the optimistic path omits key_authorization).
    let resp = if is_retriable_payment_rejection(&resp) && signer.has_stored_key_authorization() {
        if http.log_enabled() {
            eprintln!(
                "Server rejected payment (HTTP {}), retrying with key authorization...",
                resp.status_code
            );
            if let Ok(body) = resp.body_string() {
                eprintln!("Rejection body: {}", sanitize_for_terminal(&body));
            }
        }
        let provisioning_signer =
            signer
                .with_key_authorization()
                .ok_or_else(|| KeyError::SigningOperation {
                    operation: "key provisioning",
                    reason: "stored key authorization could not be applied to signing mode"
                        .to_string(),
                })?;
        let retry_provider = mpp::client::TempoProvider::new(
            provisioning_signer.signer.clone(),
            resolved.rpc_url.as_str(),
        )
        .map_err(|source| ConfigError::ProviderInitSource {
            provider: "tempo payment provider (provisioning retry)",
            source: Box::new(source),
        })?
        .with_client_id(CLIENT_ID)
        .with_signing_mode(provisioning_signer.signing_mode);
        let original_resp_rejection = parse_payment_rejection(&resp);
        let retry_credential = retry_provider
            .pay(challenge)
            .await
            .map_err(|_| original_resp_rejection)?;
        submit_credential(http, url, &retry_credential).await?
    } else {
        resp
    };

    if resp.status_code >= 400 {
        return Err(parse_payment_rejection(&resp).into());
    }

    let tx_hash = match resp.header("payment-receipt") {
        Some(header) => {
            if let Err(source) = mpp::parse_receipt(header) {
                eprintln!(
                    "Warning: ignoring invalid Payment-Receipt on paid charge response: {source}"
                );
            }
            mpp::protocol::core::extract_tx_hash(header)
                .or_else(|| mpp::parse_receipt(header).ok().map(|r| r.reference))
        }
        None => {
            eprintln!("Warning: missing Payment-Receipt on successful paid charge response");
            None
        }
    };

    Ok(PaymentResult {
        tx_hash,
        channel_id: None,
        status_code: resp.status_code,
        response: Some(resp),
    })
}

/// Handle a zero-amount charge by signing an EIP-712 proof credential.
///
/// Zero-amount charges prove wallet ownership without moving funds.
/// The mpp SDK's `TempoProvider::pay()` returns a proof credential
/// automatically when the challenge amount is zero.
async fn handle_zero_amount_charge(
    http: &HttpClient,
    url: &str,
    resolved: &ResolvedChallenge,
    signer: &Signer,
) -> Result<PaymentResult, TempoError> {
    let challenge = &resolved.challenge;

    if http.log_enabled() {
        eprintln!("Zero-amount charge: signing proof credential (no transaction)");
    }

    if http.dry_run {
        eprintln!("[DRY RUN] Proof credential ready, skipping submission.");
        return Ok(PaymentResult {
            tx_hash: None,
            channel_id: None,
            status_code: 200,
            response: None,
        });
    }

    let provider =
        mpp::client::TempoProvider::new(signer.signer.clone(), resolved.rpc_url.as_str())
            .map_err(|source| ConfigError::ProviderInitSource {
                provider: "tempo payment provider",
                source: Box::new(source),
            })?
            .with_client_id(CLIENT_ID)
            .with_signing_mode(signer.signing_mode.clone());

    let credential = provider
        .pay(challenge)
        .await
        .map_err(|e| classify_payment_error(e, &resolved.network_id))?;

    let resp = submit_credential(http, url, &credential).await?;

    if resp.status_code >= 400 {
        return Err(parse_payment_rejection(&resp).into());
    }

    Ok(PaymentResult {
        tx_hash: None,
        channel_id: None,
        status_code: resp.status_code,
        response: Some(resp),
    })
}

/// Format the credential as an Authorization header and submit to the server.
async fn submit_credential(
    http: &HttpClient,
    url: &str,
    credential: &mpp::protocol::core::PaymentCredential,
) -> Result<HttpResponse, TempoError> {
    let auth_header = mpp::format_authorization(credential).map_err(|source| {
        PaymentError::ChallengeFormatSource {
            context: "Authorization header",
            source: Box::new(source),
        }
    })?;
    let headers = vec![("Authorization".to_string(), auth_header)];
    http.execute(url, &headers).await
}

/// Maximum characters to include in a payment rejection reason.
const MAX_REJECTION_REASON_CHARS: usize = 500;

/// Parse a non-200 response after payment submission into a descriptive error.
fn parse_payment_rejection(response: &HttpResponse) -> PaymentError {
    let raw_reason = match response.body_string() {
        Ok(body) if !body.trim().is_empty() => {
            body.chars().take(MAX_REJECTION_REASON_CHARS).collect()
        }
        _ => format!("HTTP {}", response.status_code),
    };
    let reason = sanitize_for_terminal(&raw_reason);

    PaymentError::PaymentRejected {
        reason,
        status_code: response.status_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_payment_rejection_returns_full_json_body() {
        let body = br#"{"error":"insufficient funds","details":"need 0.05 USDC"}"#;
        let resp = HttpResponse::for_test(400, body);
        let err = parse_payment_rejection(&resp);
        match err {
            PaymentError::PaymentRejected {
                reason,
                status_code,
            } => {
                assert!(reason.contains("insufficient funds"));
                assert!(reason.contains("need 0.05 USDC"));
                assert_eq!(status_code, 400);
            }
            _ => panic!("expected PaymentRejected"),
        }
    }

    #[test]
    fn test_parse_payment_rejection_plain_text() {
        let body = b"Transaction reverted";
        let resp = HttpResponse::for_test(500, body);
        let err = parse_payment_rejection(&resp);
        match err {
            PaymentError::PaymentRejected { reason, .. } => {
                assert_eq!(reason, "Transaction reverted");
            }
            _ => panic!("expected PaymentRejected"),
        }
    }

    #[test]
    fn test_parse_payment_rejection_truncated() {
        let body = "a".repeat(600);
        let resp = HttpResponse::for_test(500, body.as_bytes());
        let err = parse_payment_rejection(&resp);
        match err {
            PaymentError::PaymentRejected { reason, .. } => {
                assert_eq!(reason.len(), MAX_REJECTION_REASON_CHARS);
            }
            _ => panic!("expected PaymentRejected"),
        }
    }

    #[test]
    fn test_parse_payment_rejection_empty_body() {
        let resp = HttpResponse::for_test(500, b"");
        let err = parse_payment_rejection(&resp);
        match err {
            PaymentError::PaymentRejected { reason, .. } => {
                assert_eq!(reason, "HTTP 500");
            }
            _ => panic!("expected PaymentRejected"),
        }
    }

    #[test]
    fn test_parse_payment_rejection_whitespace_body() {
        let resp = HttpResponse::for_test(503, b"   \n\t  ");
        let err = parse_payment_rejection(&resp);
        match err {
            PaymentError::PaymentRejected { reason, .. } => {
                assert_eq!(reason, "HTTP 503");
            }
            _ => panic!("expected PaymentRejected"),
        }
    }

    #[test]
    fn test_parse_payment_rejection_invalid_utf8() {
        let resp = HttpResponse::for_test(500, &[0xff, 0xfe, 0xfd]);
        let err = parse_payment_rejection(&resp);
        match err {
            PaymentError::PaymentRejected { reason, .. } => {
                assert_eq!(reason, "HTTP 500");
            }
            _ => panic!("expected PaymentRejected"),
        }
    }

    #[test]
    fn test_parse_payment_rejection_keeps_inactive_access_key_shape_as_payment_rejected() {
        let body = br#"{"success":false,"error":"MPP payment failed: Payment verification failed: Missing or invalid parameters. URL: https://rpc.mainnet.tempo.xyz Request body: {\"method\":\"eth_sendRawTransactionSync\"}"}"#;
        let resp = HttpResponse::for_test(402, body);
        let err = parse_payment_rejection(&resp);
        assert!(matches!(err, PaymentError::PaymentRejected { .. }));
    }

    #[test]
    fn test_payment_proof_invalid_wrapper_retries_key_authorization() {
        let resp = HttpResponse::for_test(
            400,
            br#"{"error":"payment_proof_invalid","message":"invalid payment proof"}"#,
        );
        assert!(is_retriable_payment_rejection(&resp));
    }

    #[test]
    fn test_unrelated_client_rejection_does_not_retry_key_authorization() {
        let resp = HttpResponse::for_test(
            400,
            br#"{"error":"invalid_request","message":"missing input"}"#,
        );
        assert!(!is_retriable_payment_rejection(&resp));
    }
}
