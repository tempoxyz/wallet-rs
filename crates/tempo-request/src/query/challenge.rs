//! Parsing and validation of 402 payment challenges.

use mpp::protocol::methods::tempo::TempoChargeExt;

use crate::{
    http::HttpResponse,
    payment::challenge::{decode_session_request, require_session_chain_id},
};
use tempo_common::{
    cli::{format::format_token_amount, terminal::sanitize_for_terminal},
    error::{PaymentError, TempoError},
    keys::Keystore,
    network::NetworkId,
};

/// Split a `WWW-Authenticate` header value that may contain multiple merged
/// `Payment` challenges into individual challenge strings.
///
/// Servers may merge multiple challenges into a single header per RFC 9110 §11.6.1,
/// e.g. `Payment id="a", …, Payment id="b", …`.
fn split_payment_challenges(header: &str) -> Vec<&str> {
    let bytes = header.as_bytes();
    let mut starts = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Skip over quoted strings so embedded "payment " doesn't cause a false split.
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1; // skip escaped char
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // closing quote
            }
            continue;
        }
        if header[i..].len() >= 8 && header[i..i + 8].eq_ignore_ascii_case("payment ") {
            starts.push(i);
        }
        i += 1;
    }
    if starts.len() <= 1 {
        return vec![header];
    }
    starts
        .iter()
        .enumerate()
        .map(|(idx, &start)| {
            let end = starts.get(idx + 1).copied().unwrap_or(header.len());
            header[start..end].trim_end_matches(|c: char| c == ',' || c.is_whitespace())
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedPaymentMethod {
    Tempo,
}

impl SupportedPaymentMethod {
    fn parse(value: &str) -> Option<Self> {
        value.eq_ignore_ascii_case("tempo").then_some(Self::Tempo)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedPaymentIntent {
    Session,
    Charge,
}

impl SupportedPaymentIntent {
    fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("session") {
            Some(Self::Session)
        } else if value.eq_ignore_ascii_case("charge") {
            Some(Self::Charge)
        } else {
            None
        }
    }
}

/// Parsed payment challenge extracted from a 402 response.
#[derive(Clone)]
pub(crate) struct ParsedChallenge {
    pub(crate) is_session: bool,
    pub(crate) network: NetworkId,
    pub(crate) amount: String,
    pub(crate) currency: String,
    pub(crate) challenge: mpp::PaymentChallenge,
}

impl ParsedChallenge {
    pub(crate) const fn intent_str(&self) -> &'static str {
        if self.is_session {
            "session"
        } else {
            "charge"
        }
    }

    /// Format the payment amount for human display, falling back to raw value + symbol.
    pub(crate) fn amount_display(&self) -> String {
        self.amount
            .parse::<u128>()
            .ok()
            .map(|a| format_token_amount(a, self.network))
            .unwrap_or_else(|| {
                sanitize_for_terminal(&format!("{} {}", self.amount, self.network.token().symbol))
            })
    }
}

/// Decode a single raw `PaymentChallenge` into a `ParsedChallenge`.
///
/// Returns `Err` with a precise message (missing chainId, unsupported intent,
/// undecodable payload, …) so that single-challenge 402s preserve the
/// historical error surface. The caller iterates over all challenges and only
/// surfaces the first error if no candidate decodes successfully.
fn try_decode(raw: &mpp::PaymentChallenge) -> Result<ParsedChallenge, TempoError> {
    SupportedPaymentMethod::parse(&raw.method.to_string())
        .ok_or_else(|| PaymentError::UnsupportedPaymentMethod(raw.method.to_string()))?;
    let intent = SupportedPaymentIntent::parse(&raw.intent.to_string())
        .ok_or_else(|| PaymentError::UnsupportedPaymentIntent(raw.intent.to_string()))?;

    let (is_session, network, amount, currency) = match intent {
        SupportedPaymentIntent::Session => {
            let session = decode_session_request(raw)?;
            let chain_id = require_session_chain_id(&session, "session request methodDetails")?;
            (
                true,
                require_chain(Some(chain_id))?,
                session.amount,
                session.currency,
            )
        }
        SupportedPaymentIntent::Charge => {
            let charge = raw.request.decode::<mpp::ChargeRequest>().map_err(|_| {
                PaymentError::ChallengeUnsupportedPayload {
                    context: "payment challenge payload",
                }
            })?;
            (
                false,
                require_chain(charge.chain_id())?,
                charge.amount,
                charge.currency,
            )
        }
    };

    Ok(ParsedChallenge {
        is_session,
        network,
        amount,
        currency,
        challenge: raw.clone(),
    })
}

/// Format the set of `(network, currency)` offered in a 402 for error display.
fn format_offered(candidates: &[ParsedChallenge]) -> String {
    if candidates.is_empty() {
        return "(none)".to_string();
    }
    candidates
        .iter()
        .map(|c| {
            format!(
                "{}/{}",
                c.network.as_str(),
                sanitize_for_terminal(&c.currency)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Format the set of `(network, currency)` the wallet has keys for, for error display.
fn format_held(keystore: &Keystore) -> String {
    if keystore.is_empty() {
        return "(no wallet configured; run 'tempo wallet login')".to_string();
    }
    let mut held: Vec<String> = keystore
        .iter()
        .flat_map(|k| {
            let network = NetworkId::from_chain_id(k.chain_id).map_or_else(
                || format!("chain {}", k.chain_id),
                |n| n.as_str().to_string(),
            );
            if k.limits.is_empty() {
                vec![format!("{network}/any")]
            } else {
                k.limits
                    .iter()
                    .map(|l| format!("{network}/{:#x}", l.currency))
                    .collect()
            }
        })
        .collect();
    held.sort();
    held.dedup();
    held.join(", ")
}

/// Parse the WWW-Authenticate header from a 402 response, pick the best
/// challenge the wallet can satisfy, and extract all payment-related context
/// needed for routing and analytics.
///
/// Selection rules, applied in order to all challenges the server offered
/// (preserving server order):
///
/// 1. Drop entries whose method/intent is unsupported or whose payload fails
///    to decode (network/currency/amount).
/// 2. If `configured_network` is set (via `--network`), drop entries on a
///    different network.
/// 3. Drop entries the wallet cannot satisfy (no key for that `(network,
///    currency)` pair). Empty and ephemeral keystores skip this pass to
///    preserve current behavior for unauthenticated and `--private-key` flows.
/// 4. Return the first survivor in server order.
///
/// Errors:
/// - `UnsupportedPaymentMethod` if no candidate survives step 1.
/// - `NoCompatibleChallenge` if at least one candidate exists but none survive
///   steps 2–3.
pub(crate) fn parse_payment_challenge(
    response: &HttpResponse,
    keystore: &Keystore,
    configured_network: Option<NetworkId>,
) -> Result<ParsedChallenge, TempoError> {
    let www_auth = response
        .header("www-authenticate")
        .ok_or_else(|| PaymentError::MissingHeader("WWW-Authenticate".to_string()))?;

    // Split merged challenges (RFC 9110 §11.6.1) and decode each.
    let parts = split_payment_challenges(www_auth);
    let raw_challenges: Vec<_> = mpp::parse_www_authenticate_all(parts)
        .into_iter()
        .filter_map(|r| r.ok())
        .collect();

    // Decode every challenge. Successful decodes become candidates; the first
    // decode error is held in reserve so single-challenge 402s can surface it
    // verbatim when no candidate survives.
    let mut candidates: Vec<ParsedChallenge> = Vec::new();
    let mut first_decode_err: Option<TempoError> = None;
    for raw in &raw_challenges {
        match try_decode(raw) {
            Ok(c) => candidates.push(c),
            Err(e) => {
                if first_decode_err.is_none() {
                    first_decode_err = Some(e);
                }
            }
        }
    }

    if candidates.is_empty() {
        // Preserve the precise decode error for single-challenge responses;
        // fall back to the historical aggregate "unsupported method" message
        // when nothing decodable was present.
        if let Some(err) = first_decode_err {
            return Err(err);
        }
        let methods: Vec<_> = raw_challenges
            .iter()
            .map(|c| c.method.to_string())
            .collect();
        return Err(PaymentError::UnsupportedPaymentMethod(methods.join(", ")).into());
    }

    let chosen = candidates.iter().find(|c| {
        configured_network.is_none_or(|n| n == c.network)
            && keystore.has_key_for_network_and_currency(c.network, &c.currency)
    });

    if let Some(c) = chosen {
        return Ok(c.clone());
    }

    Err(PaymentError::NoCompatibleChallenge {
        offered: format_offered(&candidates),
        held: format_held(keystore),
    }
    .into())
}

/// Resolve a chain ID to a known `NetworkId`, or fail with a clear error.
fn require_chain(chain_id: Option<u64>) -> Result<NetworkId, TempoError> {
    let cid = chain_id.ok_or(PaymentError::ChallengeMissingField {
        context: "payment request",
        field: "chainId",
    })?;
    NetworkId::from_chain_id(cid).ok_or_else(|| {
        PaymentError::ChallengeUnsupportedChainId {
            context: "payment request",
            chain_id: cid,
        }
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_challenge(intent: &str) -> mpp::PaymentChallenge {
        let request = mpp::Base64UrlJson::from_value(
            &serde_json::json!({"amount": "1000", "currency": "USDC.e"}),
        )
        .unwrap();
        mpp::PaymentChallenge::new("test-id", "test-realm", "tempo", intent, request)
    }

    fn make_challenge_with_payload(
        intent: &str,
        payload: serde_json::Value,
    ) -> mpp::PaymentChallenge {
        let request = mpp::Base64UrlJson::from_value(&payload).unwrap();
        mpp::PaymentChallenge::new("test-id", "test-realm", "tempo", intent, request)
    }

    fn make_ctx(is_session: bool, amount: &str) -> ParsedChallenge {
        let intent = if is_session { "session" } else { "charge" };
        ParsedChallenge {
            is_session,
            network: NetworkId::default(),
            amount: amount.to_string(),
            currency: "USDC.e".to_string(),
            challenge: make_challenge(intent),
        }
    }

    #[test]
    fn test_intent_str_session() {
        let ctx = make_ctx(true, "1000");
        assert_eq!(ctx.intent_str(), "session");
    }

    #[test]
    fn test_intent_str_charge() {
        let ctx = make_ctx(false, "1000");
        assert_eq!(ctx.intent_str(), "charge");
    }

    #[test]
    fn test_amount_display_valid_numeric() {
        let ctx = make_ctx(false, "1000000");
        let display = ctx.amount_display();
        assert!(!display.is_empty());
    }

    #[test]
    fn test_amount_display_non_numeric_fallback() {
        let ctx = make_ctx(false, "not-a-number");
        let display = ctx.amount_display();
        assert!(display.contains("not-a-number"));
    }

    #[test]
    fn test_require_chain_known_network() {
        let result = require_chain(Some(4217));
        assert!(result.is_ok());
    }

    #[test]
    fn test_require_chain_unknown_network() {
        let result = require_chain(Some(99999));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported chainId"));
    }

    #[test]
    fn test_require_chain_missing() {
        let result = require_chain(None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing chainId"));
    }

    // --- split_payment_challenges unit tests ---

    #[test]
    fn test_split_single_challenge() {
        let header =
            r#"Payment id="a", realm="api", method="tempo", intent="charge", request="e30""#;
        let parts = split_payment_challenges(header);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], header);
    }

    #[test]
    fn test_split_two_merged_challenges() {
        let header = concat!(
            r#"Payment id="a", realm="api", method="tempo", intent="charge", request="e30", "#,
            r#"Payment id="b", realm="api", method="stripe", intent="charge", request="e30""#,
        );
        let parts = split_payment_challenges(header);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("Payment id=\"a\""));
        assert!(parts[1].starts_with("Payment id=\"b\""));
        // Trailing comma/whitespace should be trimmed from the first chunk
        assert!(!parts[0].ends_with(','));
    }

    #[test]
    fn test_split_case_insensitive() {
        let header = concat!(
            r#"PAYMENT id="a", realm="api", method="tempo", intent="charge", request="e30", "#,
            r#"payment id="b", realm="api", method="stripe", intent="charge", request="e30""#,
        );
        let parts = split_payment_challenges(header);
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn test_split_ignores_payment_inside_quotes() {
        // "payment " inside a quoted realm value must not cause a false split.
        let header = r#"Payment id="a", realm="My payment service", method="tempo", intent="charge", request="e30""#;
        let parts = split_payment_challenges(header);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], header);
    }

    #[test]
    fn test_split_no_payment_scheme() {
        let header = "Bearer token123";
        let parts = split_payment_challenges(header);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], header);
    }

    // --- parse_payment_challenge with merged headers ---

    /// Address constants used in challenge selection tests.
    const PATHUSD: &str = "0x20c0000000000000000000000000000000000000";
    const USDC_E: &str = "0x20c000000000000000000000b9537d11c60e8b50";

    fn tempo_charge_challenge(id: &str, chain_id: u64, currency: &str) -> mpp::PaymentChallenge {
        mpp::PaymentChallenge::new(
            id,
            "realm",
            "tempo",
            "charge",
            mpp::Base64UrlJson::from_value(&serde_json::json!({
                "amount": "1000",
                "currency": currency,
                "payTo": "0x1111111111111111111111111111111111111111",
                "methodDetails": { "chainId": chain_id }
            }))
            .unwrap(),
        )
    }

    /// Build a keystore with a single key configured for `(chain_id, currency)`.
    ///
    /// Goes through the public TOML deserialization path so we don't have to
    /// reach into private `keys::model` types from a downstream crate.
    fn keystore_with_key(chain_id: u64, currency: &str) -> Keystore {
        let toml = format!(
            r#"
[[keys]]
wallet_type = "local"
wallet_address = "0x1111111111111111111111111111111111111111"
chain_id = {chain_id}
key = "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318"
key_address = "0x1111111111111111111111111111111111111111"

[[keys.limits]]
currency = "{currency}"
limit = "1000000"
"#
        );
        toml::from_str(&toml).expect("test keystore TOML")
    }

    #[test]
    fn test_parse_payment_challenge_merged_selects_tempo() {
        let tempo = tempo_charge_challenge("tempo-id", 4217, PATHUSD);
        let stripe = mpp::PaymentChallenge::new(
            "stripe-id",
            "realm",
            "stripe",
            "charge",
            mpp::Base64UrlJson::from_value(&serde_json::json!({"amount": "100"})).unwrap(),
        );
        let merged = format!(
            "{}, {}",
            mpp::format_www_authenticate(&tempo).unwrap(),
            mpp::format_www_authenticate(&stripe).unwrap(),
        );
        let response =
            HttpResponse::for_test_with_headers(402, b"", &[("www-authenticate", &merged)]);
        let parsed = parse_payment_challenge(&response, &Keystore::default(), None).unwrap();
        assert!(!parsed.is_session);
        assert_eq!(parsed.challenge.id, "tempo-id");
    }

    /// QuickNode-shaped 402: server offers moderato first, mainnet PathUSD,
    /// then mainnet USDC.e. A wallet funded only for mainnet USDC.e should pick
    /// the third challenge instead of failing on moderato.
    #[test]
    fn test_parse_payment_challenge_skips_to_wallet_compatible() {
        let moderato = tempo_charge_challenge("moderato", 42431, PATHUSD);
        let mainnet_path = tempo_charge_challenge("mainnet-path", 4217, PATHUSD);
        let mainnet_usdc = tempo_charge_challenge("mainnet-usdc", 4217, USDC_E);
        let merged = format!(
            "{}, {}, {}",
            mpp::format_www_authenticate(&moderato).unwrap(),
            mpp::format_www_authenticate(&mainnet_path).unwrap(),
            mpp::format_www_authenticate(&mainnet_usdc).unwrap(),
        );
        let response =
            HttpResponse::for_test_with_headers(402, b"", &[("www-authenticate", &merged)]);

        let keystore = keystore_with_key(4217, USDC_E);
        let parsed = parse_payment_challenge(&response, &keystore, None).unwrap();
        assert_eq!(parsed.challenge.id, "mainnet-usdc");
        assert_eq!(parsed.network, NetworkId::Tempo);
    }

    /// With `-n tempo` set, moderato should be filtered out silently rather
    /// than producing the legacy "Server requested network 'tempo-moderato'
    /// but --network is 'tempo'" error.
    #[test]
    fn test_parse_payment_challenge_network_filter_skips_other_networks() {
        let moderato = tempo_charge_challenge("moderato", 42431, PATHUSD);
        let mainnet = tempo_charge_challenge("mainnet", 4217, PATHUSD);
        let merged = format!(
            "{}, {}",
            mpp::format_www_authenticate(&moderato).unwrap(),
            mpp::format_www_authenticate(&mainnet).unwrap(),
        );
        let response =
            HttpResponse::for_test_with_headers(402, b"", &[("www-authenticate", &merged)]);

        let parsed =
            parse_payment_challenge(&response, &Keystore::default(), Some(NetworkId::Tempo))
                .unwrap();
        assert_eq!(parsed.challenge.id, "mainnet");
        assert_eq!(parsed.network, NetworkId::Tempo);
    }

    /// When the wallet has no key matching any offered challenge, return a
    /// `NoCompatibleChallenge` error listing what was offered and what we hold.
    #[test]
    fn test_parse_payment_challenge_no_compatible_returns_helpful_error() {
        let moderato = tempo_charge_challenge("moderato", 42431, PATHUSD);
        let www_auth = mpp::format_www_authenticate(&moderato).unwrap();
        let response =
            HttpResponse::for_test_with_headers(402, b"", &[("www-authenticate", &www_auth)]);

        // Wallet has a mainnet USDC.e key — incompatible with the moderato offer.
        let keystore = keystore_with_key(4217, USDC_E);
        let err = match parse_payment_challenge(&response, &keystore, None) {
            Ok(_) => panic!("expected NoCompatibleChallenge"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(msg.contains("No payment challenge matches"), "got: {msg}");
        assert!(
            msg.contains("tempo-moderato"),
            "should list moderato as offered: {msg}"
        );
        assert!(
            msg.contains("Wallet has keys for: tempo/"),
            "should list mainnet as held: {msg}"
        );
    }

    #[test]
    fn test_parse_payment_challenge_merged_no_supported_method() {
        let stripe = mpp::PaymentChallenge::new(
            "stripe-id",
            "realm",
            "stripe",
            "charge",
            mpp::Base64UrlJson::from_value(&serde_json::json!({"amount": "100"})).unwrap(),
        );
        let www_auth = mpp::format_www_authenticate(&stripe).unwrap();
        let response =
            HttpResponse::for_test_with_headers(402, b"", &[("www-authenticate", &www_auth)]);
        let err = match parse_payment_challenge(&response, &Keystore::default(), None) {
            Ok(_) => panic!("expected unsupported method to fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("stripe"),
            "error should mention the unsupported method: {err}"
        );
    }

    #[test]
    fn test_parse_payment_challenge_missing_header() {
        let response = HttpResponse::for_test(402, b"");
        let result = parse_payment_challenge(&response, &Keystore::default(), None);
        let err = result.err().expect("should be an error");
        assert!(err.to_string().contains("WWW-Authenticate"));
    }

    #[test]
    fn test_parse_payment_challenge_session_missing_chainid_is_rejected() {
        let request = mpp::Base64UrlJson::from_value(&serde_json::json!({
            "amount": "1000",
            "currency": "0x20c0000000000000000000000000000000000000",
            "recipient": "0x1111111111111111111111111111111111111111",
            "methodDetails": {
                "escrowContract": tempo_common::network::TEMPO_MODERATO_ESCROW.to_string()
            }
        }))
        .unwrap();
        let challenge = mpp::PaymentChallenge::new("id", "realm", "tempo", "session", request);
        let www_auth = mpp::format_www_authenticate(&challenge).unwrap();

        let response =
            HttpResponse::for_test_with_headers(402, b"", &[("www-authenticate", &www_auth)]);
        let err = match parse_payment_challenge(&response, &Keystore::default(), None) {
            Ok(_) => panic!("expected session challenge without chainId to fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("missing chainId"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_payment_challenge_unknown_intent_is_rejected_for_charge_payload() {
        let challenge = make_challenge_with_payload(
            "unexpected",
            serde_json::json!({
                "amount": "1000",
                "currency": "0x20c0000000000000000000000000000000000000",
                "payTo": "0x1111111111111111111111111111111111111111",
                "methodDetails": {
                    "chainId": 4217,
                    "token": "0x20c0000000000000000000000000000000000000"
                }
            }),
        );
        let www_auth = mpp::format_www_authenticate(&challenge).unwrap();
        let response =
            HttpResponse::for_test_with_headers(402, b"", &[("www-authenticate", &www_auth)]);

        let err = match parse_payment_challenge(&response, &Keystore::default(), None) {
            Ok(_) => panic!("expected unsupported intent to fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("Unsupported payment intent"));
    }

    #[test]
    fn test_parse_payment_challenge_unknown_intent_is_rejected_for_session_payload() {
        let challenge = make_challenge_with_payload(
            "unexpected",
            serde_json::json!({
                "amount": "1000",
                "currency": "0x20c0000000000000000000000000000000000000",
                "recipient": "0x1111111111111111111111111111111111111111",
                "methodDetails": {
                    "chainId": 4217,
                    "escrowContract": tempo_common::network::TEMPO_MODERATO_ESCROW.to_string()
                }
            }),
        );
        let www_auth = mpp::format_www_authenticate(&challenge).unwrap();
        let response =
            HttpResponse::for_test_with_headers(402, b"", &[("www-authenticate", &www_auth)]);

        let err = match parse_payment_challenge(&response, &Keystore::default(), None) {
            Ok(_) => panic!("expected unsupported intent to fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("Unsupported payment intent"));
    }
}
