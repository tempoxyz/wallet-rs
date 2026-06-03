//! Payment routing: route 402 flows to charge or session payment paths.
//!
//! This module is crate-internal and intentionally decoupled from CLI types.

use alloy::primitives::Address;
use mpp::PaymentChallenge;

use crate::http::HttpClient;
use tempo_common::{
    config::Config,
    error::{KeyError, TempoError},
    keys::{Keystore, Signer},
    network::NetworkId,
};

const KEY_NOT_FOUND_SELECTOR: &str = "0x5f3f479c";
const KEY_ALREADY_EXISTS_SELECTOR: &str = "0xaa1ba2f8";

use super::{
    charge::handle_charge_request,
    session::handle_session_request,
    types::{PaymentResult, ResolvedChallenge},
};

/// Dispatch to charge or session payment flow.
///
/// `network` is the already-resolved network from the 402 challenge.
/// The caller is responsible for parsing the challenge and extracting
/// the network before calling this function (see `query/challenge.rs`).
///
/// The `--network` filter (when set on `http`) is enforced upstream during
/// challenge selection in `parse_payment_challenge`, so any `network` reaching
/// this function is guaranteed to match `http.network` if it is `Some`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_payment(
    config: &Config,
    http: &HttpClient,
    is_session: bool,
    url: &str,
    challenge: PaymentChallenge,
    network: NetworkId,
    keys: &Keystore,
) -> Result<PaymentResult, TempoError> {
    debug_assert!(
        http.network.is_none_or(|allowed| allowed == network),
        "challenge selection should have filtered to --network already"
    );

    let rpc_url = config.rpc_url(network);
    let resolved = ResolvedChallenge {
        challenge,
        network_id: network,
        rpc_url,
    };

    let signer = keys.signer(resolved.network_id)?;
    let signer = preflight_signer_key_state(config, keys, resolved.network_id, signer).await?;

    if is_session {
        return handle_session_request(http, url, resolved, signer, keys).await;
    }

    handle_charge_request(http, url, resolved, signer).await
}

async fn preflight_signer_key_state(
    config: &Config,
    keys: &Keystore,
    network: NetworkId,
    signer: Signer,
) -> Result<Signer, TempoError> {
    let Some(entry) = keys.key_for_network(network) else {
        return Ok(signer);
    };
    if keys.ephemeral
        || entry.is_direct_eoa_key()
        || entry.provisioned
        || !signer.has_stored_key_authorization()
    {
        return Ok(signer);
    }

    let Some(wallet_address) = entry.wallet_address_parsed() else {
        return Ok(signer);
    };
    let Some(key_address) = entry.key_address_parsed() else {
        return Ok(signer);
    };

    let provider = alloy::providers::ProviderBuilder::new().connect_http(config.rpc_url(network));
    let token = network.token();

    match mpp::client::tempo::signing::keychain::query_key_spending_limit(
        &provider,
        wallet_address,
        key_address,
        token.address,
    )
    .await
    {
        Ok(_) => {
            mark_key_provisioned(keys, wallet_address, network)?;
            Ok(signer)
        }
        Err(err) => match classify_key_preflight_error(&err) {
            KeyPreflightErrorState::NotProvisioned => signer_with_key_authorization(&signer),
            KeyPreflightErrorState::Provisioned => {
                tracing::warn!(
                    error = %err,
                    "key provisioning preflight found a provisioned key but failed to query spending limits"
                );
                mark_key_provisioned(keys, wallet_address, network)?;
                Ok(signer)
            }
            KeyPreflightErrorState::Unknown => {
                tracing::warn!(
                    error = %err,
                    "key provisioning preflight failed; continuing with optimistic signing"
                );
                Ok(signer)
            }
        },
    }
}

fn signer_with_key_authorization(signer: &Signer) -> Result<Signer, TempoError> {
    signer.with_key_authorization().ok_or_else(|| {
        KeyError::SigningOperation {
            operation: "key provisioning preflight",
            reason: "stored key authorization could not be applied to signing mode".to_string(),
        }
        .into()
    })
}

fn mark_key_provisioned(
    keys: &Keystore,
    wallet_address: Address,
    network: NetworkId,
) -> Result<(), TempoError> {
    let mut persisted = keys.clone();
    if persisted.mark_provisioned_address(wallet_address, network.chain_id()) {
        persisted.save()?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyPreflightErrorState {
    NotProvisioned,
    Provisioned,
    Unknown,
}

fn classify_key_preflight_error(err: &mpp::MppError) -> KeyPreflightErrorState {
    if matches!(
        err,
        mpp::MppError::Tempo(mpp::client::TempoClientError::AccessKeyNotProvisioned)
    ) {
        return KeyPreflightErrorState::NotProvisioned;
    }

    let message = err.to_string().to_lowercase();
    if message.contains("not provisioned")
        || message.contains("keynotfound")
        || message.contains(KEY_NOT_FOUND_SELECTOR)
    {
        return KeyPreflightErrorState::NotProvisioned;
    }

    if message.contains("failed to query remaining limit")
        || message.contains("keyalreadyexists")
        || message.contains(KEY_ALREADY_EXISTS_SELECTOR)
    {
        return KeyPreflightErrorState::Provisioned;
    }

    KeyPreflightErrorState::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_classifies_typed_not_provisioned() {
        let err = mpp::MppError::Tempo(mpp::client::TempoClientError::AccessKeyNotProvisioned);
        assert_eq!(
            classify_key_preflight_error(&err),
            KeyPreflightErrorState::NotProvisioned
        );
    }

    #[test]
    fn preflight_classifies_key_not_found_selector_as_not_provisioned() {
        let err = mpp::MppError::Http(
            "failed to query key info: execution reverted, data: \"0x5f3f479c\"".to_string(),
        );
        assert_eq!(
            classify_key_preflight_error(&err),
            KeyPreflightErrorState::NotProvisioned
        );
    }

    #[test]
    fn preflight_classifies_remaining_limit_revert_as_provisioned() {
        let err = mpp::MppError::Http(
            "failed to query remaining limit: execution reverted, data: \"0xaa4bc69a63b4290d\""
                .to_string(),
        );
        assert_eq!(
            classify_key_preflight_error(&err),
            KeyPreflightErrorState::Provisioned
        );
    }

    #[test]
    fn preflight_classifies_key_already_exists_retry_as_provisioned() {
        let err = mpp::MppError::Http(
            "gas estimation failed: KeyAlreadyExists(KeyAlreadyExists)".to_string(),
        );
        assert_eq!(
            classify_key_preflight_error(&err),
            KeyPreflightErrorState::Provisioned
        );
    }

    #[test]
    fn preflight_classifies_unknown_errors_as_unknown() {
        let err = mpp::MppError::Http("failed to reach rpc node".to_string());
        assert_eq!(
            classify_key_preflight_error(&err),
            KeyPreflightErrorState::Unknown
        );
    }
}
