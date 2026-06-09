use super::ChannelState;
use alloy::primitives::B256;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use mpp::{
    parse_receipt,
    protocol::{core::extract_tx_hash, methods::tempo::SessionReceipt},
};
use tempo_common::{
    cli::terminal::sanitize_for_terminal,
    error::{PaymentError, TempoError},
};

#[derive(Debug, Clone)]
pub(super) struct ValidatedSessionReceipt {
    pub(super) accepted_cumulative: u128,
    pub(super) server_spent: Option<u128>,
    pub(super) spent_parse_error: Option<String>,
    pub(super) tx_reference: Option<String>,
}

pub(super) fn validate_session_receipt_fields(
    receipt: &SessionReceipt,
    expected_channel_id: B256,
) -> Result<u128, String> {
    if receipt.method != "tempo" {
        return Err(format!("method must be 'tempo' (got '{}')", receipt.method));
    }
    if receipt.intent != "session" {
        return Err(format!(
            "intent must be 'session' (got '{}')",
            receipt.intent
        ));
    }
    if receipt.status != "success" {
        return Err(format!(
            "status must be 'success' (got '{}')",
            receipt.status
        ));
    }

    let receipt_channel_id = receipt
        .channel_id
        .parse::<B256>()
        .map_err(|_| format!("invalid channelId bytes32 value: {}", receipt.channel_id))?;
    if receipt_channel_id != expected_channel_id {
        return Err(format!(
            "channelId mismatch (expected {expected_channel_id:#x}, got {receipt_channel_id:#x})"
        ));
    }

    receipt
        .accepted_cumulative
        .parse::<u128>()
        .map_err(|source| {
            format!(
                "acceptedCumulative must be an integer amount (got '{}'): {source}",
                receipt.accepted_cumulative
            )
        })
}

pub(super) fn parse_validated_session_receipt_header(
    receipt_header: &str,
    expected_channel_id: B256,
) -> Result<ValidatedSessionReceipt, String> {
    let base = parse_receipt(receipt_header)
        .map_err(|source| format!("invalid Payment-Receipt header: {source}"))?;
    if base.method.as_str() != "tempo" {
        return Err(format!(
            "method must be 'tempo' (got '{}')",
            base.method.as_str()
        ));
    }
    if !base.is_success() {
        return Err(format!("status must be 'success' (got '{}')", base.status));
    }

    let decoded = URL_SAFE_NO_PAD
        .decode(receipt_header.trim())
        .map_err(|source| format!("invalid Payment-Receipt base64url payload: {source}"))?;
    let session_receipt: SessionReceipt = serde_json::from_slice(&decoded)
        .map_err(|source| format!("invalid session receipt JSON payload: {source}"))?;

    let accepted_cumulative =
        validate_session_receipt_fields(&session_receipt, expected_channel_id)?;
    let (server_spent, spent_parse_error) = match session_receipt.spent.trim().parse::<u128>() {
        Ok(value) => (Some(value), None),
        Err(source) => (
            None,
            Some(format!(
                "spent must be an integer amount (got '{}'): {source}",
                sanitize_for_terminal(&session_receipt.spent)
            )),
        ),
    };
    let tx_reference = extract_tx_hash(receipt_header)
        .or_else(|| session_receipt.tx_hash.clone())
        .or(Some(base.reference));

    Ok(ValidatedSessionReceipt {
        accepted_cumulative,
        server_spent,
        spent_parse_error,
        tx_reference,
    })
}

pub(super) fn missing_payment_receipt_error(context: &str) -> TempoError {
    PaymentError::PaymentRejected {
        reason: format!("Missing required Payment-Receipt on successful paid {context}"),
        status_code: 502,
    }
    .into()
}

pub(super) fn invalid_payment_receipt_error(context: &str, reason: &str) -> TempoError {
    let safe_reason = sanitize_for_terminal(reason);
    PaymentError::PaymentRejected {
        reason: format!(
            "Invalid required Payment-Receipt on successful paid {context}: {safe_reason}"
        ),
        status_code: 502,
    }
    .into()
}

pub(super) fn protocol_spent_error(reason: String) -> TempoError {
    PaymentError::PaymentRejected {
        reason: format!("Malformed payment protocol field: payment-receipt.spent {reason}"),
        status_code: 502,
    }
    .into()
}

pub(super) fn validate_receipt_spent_strict(
    accepted_cumulative: u128,
    spent: Option<u128>,
) -> Result<u128, TempoError> {
    let spent = spent.ok_or_else(|| protocol_spent_error("is missing".to_string()))?;

    if spent > accepted_cumulative {
        return Err(protocol_spent_error(format!(
            "must be <= acceptedCumulative (spent={spent}, acceptedCumulative={accepted_cumulative})"
        )));
    }

    Ok(spent)
}

pub(super) fn apply_receipt_amounts_strict(
    state: &mut ChannelState,
    accepted_cumulative: u128,
    spent: Option<u128>,
) -> Result<(), TempoError> {
    let spent = validate_receipt_spent_strict(accepted_cumulative, spent)?;

    state.cumulative_amount = state.cumulative_amount.max(accepted_cumulative);
    state.accepted_cumulative = state.accepted_cumulative.max(accepted_cumulative);
    state.server_spent = spent;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        apply_receipt_amounts_strict, invalid_payment_receipt_error, missing_payment_receipt_error,
        parse_validated_session_receipt_header, validate_receipt_spent_strict,
        validate_session_receipt_fields,
    };
    use alloy::primitives::{Address, B256};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use mpp::protocol::methods::tempo::SessionReceipt;

    use crate::payment::session::ChannelState;

    fn sample_receipt(channel_id: B256, accepted_cumulative: u128) -> SessionReceipt {
        SessionReceipt {
            method: "tempo".to_string(),
            intent: "session".to_string(),
            status: "success".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            reference: format!("{channel_id:#x}"),
            challenge_id: "challenge-123".to_string(),
            channel_id: format!("{channel_id:#x}"),
            accepted_cumulative: accepted_cumulative.to_string(),
            spent: accepted_cumulative.to_string(),
            units: Some(7),
            tx_hash: Some(format!("{:#x}", B256::from([0x11; 32]))),
        }
    }

    fn encode_receipt_header(receipt: &SessionReceipt) -> String {
        let json = serde_json::to_vec(receipt).unwrap();
        URL_SAFE_NO_PAD.encode(json)
    }

    #[test]
    fn parse_validated_receipt_accepts_well_formed_header() {
        let channel_id = B256::from([0x22; 32]);
        let receipt = sample_receipt(channel_id, 42);
        let header = encode_receipt_header(&receipt);

        let parsed = parse_validated_session_receipt_header(&header, channel_id).unwrap();
        assert_eq!(parsed.accepted_cumulative, 42);
        assert!(parsed.tx_reference.is_some());
    }

    #[test]
    fn validate_session_receipt_rejects_wrong_channel_id() {
        let channel_id = B256::from([0x22; 32]);
        let wrong_channel = B256::from([0x33; 32]);
        let receipt = sample_receipt(wrong_channel, 42);

        let err = validate_session_receipt_fields(&receipt, channel_id).unwrap_err();
        assert!(err.contains("channelId mismatch"));
    }

    #[test]
    fn validate_session_receipt_rejects_wrong_intent() {
        let channel_id = B256::from([0x22; 32]);
        let mut receipt = sample_receipt(channel_id, 42);
        receipt.intent = "charge".to_string();

        let err = validate_session_receipt_fields(&receipt, channel_id).unwrap_err();
        assert!(err.contains("intent must be 'session'"));
    }

    #[test]
    fn parse_validated_receipt_sanitizes_spent_parse_errors() {
        let channel_id = B256::from([0x44; 32]);
        let mut receipt = sample_receipt(channel_id, 42);
        receipt.spent = "12\u{1b}[31m".to_string();
        let header = encode_receipt_header(&receipt);

        let parsed = parse_validated_session_receipt_header(&header, channel_id)
            .expect("receipt should parse while preserving spent parse error for strict mode");
        let parse_error = parsed
            .spent_parse_error
            .expect("invalid spent should produce parse error");
        assert!(!parse_error.chars().any(char::is_control));
        assert!(parse_error.contains("12[31m"));
    }

    fn state() -> ChannelState {
        ChannelState {
            channel_id: B256::from([0x11; 32]),
            escrow_contract: Address::ZERO,
            chain_id: 4217,
            deposit: 100,
            cumulative_amount: 20,
            accepted_cumulative: 10,
            max_cumulative_spend: None,
            server_spent: 5,
            session_protocol: mpp::protocol::methods::tempo::session::SESSION_PROTOCOL_LEGACY
                .to_string(),
            descriptor: None,
        }
    }

    #[test]
    fn apply_receipt_amounts_strict_requires_spent() {
        let mut state = state();
        let err = apply_receipt_amounts_strict(&mut state, 25, None).unwrap_err();
        assert!(err.to_string().contains("payment-receipt.spent is missing"));
    }

    #[test]
    fn validate_receipt_spent_strict_requires_spent() {
        let err = validate_receipt_spent_strict(25, None).unwrap_err();
        assert!(err.to_string().contains("payment-receipt.spent is missing"));
    }

    #[test]
    fn validate_receipt_spent_strict_rejects_spent_above_accepted() {
        let err = validate_receipt_spent_strict(25, Some(30)).unwrap_err();
        assert!(err.to_string().contains("must be <= acceptedCumulative"));
    }

    #[test]
    fn validate_receipt_spent_strict_accepts_reconciled_lower_spent() {
        let spent = validate_receipt_spent_strict(25, Some(7)).unwrap();
        assert_eq!(spent, 7);
    }

    #[test]
    fn apply_receipt_amounts_strict_rejects_spent_above_accepted() {
        let mut state = state();
        let err = apply_receipt_amounts_strict(&mut state, 25, Some(30)).unwrap_err();
        assert!(err.to_string().contains("must be <= acceptedCumulative"));
    }

    #[test]
    fn apply_receipt_amounts_strict_allows_reconciled_lower_spent() {
        let mut state = state();
        state.server_spent = 10;

        let result = apply_receipt_amounts_strict(&mut state, 30, Some(7));
        assert!(result.is_ok());
        assert_eq!(state.server_spent, 7);
        assert_eq!(state.accepted_cumulative, 30);
    }

    #[test]
    fn invalid_payment_receipt_error_sanitizes_control_chars() {
        let err = invalid_payment_receipt_error("session response", "bad\u{1b}[31m");
        let msg = err.to_string();
        assert!(!msg.chars().any(char::is_control));
        assert!(msg.contains("bad[31m"));
    }

    #[test]
    fn missing_payment_receipt_error_mentions_context() {
        let err = missing_payment_receipt_error("topUp response");
        assert!(err.to_string().contains("topUp response"));
    }
}
