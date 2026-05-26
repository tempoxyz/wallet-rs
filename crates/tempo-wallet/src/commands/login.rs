//! Login command — browser-based wallet authentication (device code + PKCE flow).

use std::time::{Duration, Instant};

use alloy::{primitives::Address, providers::ProviderBuilder, signers::local::PrivateKeySigner};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use colored::Colorize;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;
use zeroize::Zeroizing;

use super::whoami::show_whoami;
use crate::{
    analytics::{self, CallbackReceivedPayload, LoginFailurePayload, WalletCreatedPayload},
    commands::auth::BrowserLaunchStatus,
};
use tempo_common::{
    cli::{context::Context, output::OutputFormat},
    error::{ConfigError, InputError, KeyError, NetworkError, TempoError},
    keys::{Keystore, WalletType},
    network::NetworkId,
    security::sanitize_error,
};

const CALLBACK_TIMEOUT_SECS: u64 = 900; // 15 minutes
const POLL_INTERVAL_SECS: u64 = 2;

pub(crate) async fn run(ctx: &Context, no_browser: bool) -> Result<(), TempoError> {
    run_impl(ctx, false, no_browser).await
}

pub(crate) async fn run_with_reauth(ctx: &Context) -> Result<(), TempoError> {
    run_impl(ctx, true, false).await
}

async fn run_impl(ctx: &Context, force_reauth: bool, no_browser: bool) -> Result<(), TempoError> {
    ctx.track_event(analytics::LOGIN_STARTED);

    let already_logged_in = ctx.keys.has_key_for_network(ctx.network);

    if force_reauth && already_logged_in {
        ensure_refresh_supported(ctx)?;
    }

    let needs_reauth = if force_reauth {
        already_logged_in
    } else if already_logged_in {
        is_key_revoked_or_expired(ctx).await
    } else {
        false
    };

    let mut stale_key_backup: Option<Keystore> = None;
    if needs_reauth {
        stale_key_backup = invalidate_stale_key(ctx, force_reauth)?;
    }

    if !already_logged_in || needs_reauth {
        let result = do_login(ctx, no_browser).await;

        if let Some(ref a) = ctx.analytics {
            track_login_result(a, &result);
        }

        if let Err(err) = result {
            restore_stale_key(ctx, stale_key_backup)?;
            return Err(err);
        }
    }

    if ctx.output_format == OutputFormat::Text {
        let msg = if force_reauth && already_logged_in {
            "\nAccess key refreshed!\n"
        } else if already_logged_in && !needs_reauth {
            "Already logged in.\n"
        } else {
            "\nWallet connected!\n"
        };
        eprintln!("{msg}");
    }

    let keys = ctx.keys.reload()?;
    show_whoami(ctx, Some(&keys), None).await
}

/// Check whether the stored access key has been revoked or has expired on-chain.
///
/// Returns `true` when the key is definitively invalid, `false` otherwise
/// (including on RPC errors — we don't want network failures to block login).
async fn is_key_revoked_or_expired(ctx: &Context) -> bool {
    let Some(key_entry) = ctx.keys.key_for_network(ctx.network) else {
        return false;
    };
    let Some(wallet_address) = key_entry.wallet_address_parsed() else {
        return false;
    };
    let Some(key_address) = key_entry.key_address_parsed() else {
        return false;
    };
    // Direct EOA keys (wallet == signer) are not keychain-managed
    if key_entry.is_direct_eoa_key() {
        return false;
    }

    let rpc_url = ctx.config.rpc_url(ctx.network);
    let provider = ProviderBuilder::new().connect_http(rpc_url);
    let token = ctx.network.token();

    match mpp::client::tempo::signing::keychain::query_key_spending_limit(
        &provider,
        wallet_address,
        key_address,
        token.address,
    )
    .await
    {
        Ok(_) => false,
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            msg.contains("revoked") || msg.contains("expired")
        }
    }
}

/// Remove a revoked/expired key so the fresh login flow can proceed.
fn invalidate_stale_key(ctx: &Context, force_reauth: bool) -> Result<Option<Keystore>, TempoError> {
    let Some(key_entry) = ctx.keys.key_for_network(ctx.network) else {
        return Ok(None);
    };
    let Some(wallet_address) = key_entry.wallet_address_parsed() else {
        return Ok(None);
    };

    let mut keys = ctx.keys.clone();
    let backup = keys.clone();
    keys.delete_passkey_wallet_address(wallet_address)?;
    keys.save()?;

    if ctx.output_format == OutputFormat::Text {
        let msg = if force_reauth {
            "Refreshing access key..."
        } else {
            "Existing access key is no longer valid. Re-authenticating..."
        };
        eprintln!("{msg}");
    }

    Ok(Some(backup))
}

fn restore_stale_key(ctx: &Context, backup: Option<Keystore>) -> Result<(), TempoError> {
    let Some(backup) = backup else {
        return Ok(());
    };

    backup.save()?;

    if ctx.output_format == OutputFormat::Text {
        eprintln!("Access key refresh failed. Restored previous access key.");
    }

    Ok(())
}

fn ensure_refresh_supported(ctx: &Context) -> Result<(), TempoError> {
    let Some(key_entry) = ctx.keys.key_for_network(ctx.network) else {
        return Ok(());
    };

    if key_entry.wallet_type == WalletType::Passkey {
        return Ok(());
    }

    Err(ConfigError::Invalid(
        "Access-key refresh is only supported for passkey wallets. Run 'tempo wallet login' to re-authorize this wallet."
            .to_string(),
    )
    .into())
}

fn track_login_result(a: &tempo_common::analytics::Analytics, result: &Result<(), TempoError>) {
    match result {
        Ok(()) => a.track_event(analytics::LOGIN_SUCCESS),
        Err(e) => {
            let is_timeout = matches!(e, TempoError::Key(KeyError::LoginExpired));
            if is_timeout {
                a.track_event(analytics::LOGIN_TIMEOUT);
            } else {
                a.track(
                    analytics::LOGIN_FAILURE,
                    LoginFailurePayload {
                        error: sanitize_error(&e.to_string()),
                    },
                );
            }
        }
    }
}

async fn do_login(ctx: &Context, no_browser: bool) -> Result<(), TempoError> {
    let auth_server_url =
        std::env::var("TEMPO_AUTH_URL").unwrap_or_else(|_| ctx.network.auth_url().to_string());

    let parsed_url = Url::parse(&auth_server_url).map_err(|source| InputError::UrlParseFor {
        context: "auth server",
        source,
    })?;
    let auth_base_url = parsed_url.origin().ascii_serialization();

    let local_signer = PrivateKeySigner::random();
    let uncompressed = local_signer
        .credential()
        .verifying_key()
        .to_encoded_point(false);
    let pub_key = format!("0x{}", hex::encode(uncompressed.as_bytes()));

    let (code_verifier, code_challenge) = generate_pkce_pair()?;

    let client = reqwest::Client::builder()
        .build()
        .map_err(NetworkError::Reqwest)?;
    let code = create_device_code(
        &client,
        &auth_base_url,
        &pub_key,
        &code_challenge,
        ctx.network,
    )
    .await?;

    let mut auth_url = parsed_url;
    set_auth_query_param(&mut auth_url, "network", ctx.network.auth_network_slug());
    set_auth_query_param(
        &mut auth_url,
        "chain_id",
        &ctx.network.chain_id().to_string(),
    );
    set_auth_query_param(&mut auth_url, "code", &code);
    let url_str = auth_url.to_string();

    // Always print a manual fallback URL, even in machine output modes.
    eprintln!("Auth URL: {url_str}");

    // Always attempt browser open, even in machine output modes.
    // Some agents run login with non-text output (`-t`/JSON) and still need
    // the browser flow to start.
    let browser_launch_status = super::auth::try_open_browser(&url_str, no_browser);

    if no_browser {
        show_remote_login_prompt(ctx.network, &url_str, &code);
    } else if ctx.output_format == OutputFormat::Text {
        show_login_prompt(ctx.network, &code);
    }

    if should_track_callback_window(browser_launch_status) {
        ctx.track_event(analytics::CALLBACK_WINDOW_OPENED);
    }

    let callback =
        poll_until_authorized(&client, &auth_base_url, &code, &code_verifier, ctx.network).await?;

    ctx.track(
        analytics::CALLBACK_RECEIVED,
        CallbackReceivedPayload {
            duration_secs: callback.duration_secs,
        },
    );

    save_keys(ctx.network, &ctx.keys, callback, local_signer)?;

    ctx.track(
        analytics::WALLET_CREATED,
        WalletCreatedPayload {
            wallet_type: "passkey".to_string(),
        },
    );
    ctx.track_event(analytics::KEY_CREATED);
    if let Some(ref a) = ctx.analytics {
        a.identify(&ctx.keys);
    }

    Ok(())
}

/// Display the verification code and wait prompt for authentication.
fn show_login_prompt(network: NetworkId, code: &str) {
    let display_code = format_verification_code(code);
    eprintln!("Network: {}", network.auth_network_slug().bold());
    eprintln!("Verification code: {}", display_code.bold());
    eprintln!();
    eprintln!("Waiting for authentication...");
}

/// Display the remote-host handoff prompt for a user who is chatting from another device.
fn show_remote_login_prompt(network: NetworkId, auth_url: &str, code: &str) {
    let prompt = remote_login_prompt(network, auth_url, code);
    eprintln!("{}", prompt.network_line);
    eprintln!("{}", prompt.auth_url_line);
    eprintln!("Verification code: {}", prompt.verification_code.bold());
    eprintln!("{}", prompt.continue_line);
    eprintln!("{}", prompt.return_line);
    eprintln!();
    eprintln!("Waiting for authentication...");
}

struct RemoteLoginPrompt {
    network_line: String,
    auth_url_line: String,
    verification_code: String,
    continue_line: &'static str,
    return_line: &'static str,
}

fn remote_login_prompt(network: NetworkId, auth_url: &str, code: &str) -> RemoteLoginPrompt {
    RemoteLoginPrompt {
        network_line: format!("Network: {}", network.auth_network_slug()),
        auth_url_line: format!("Open this link on your device: {auth_url}"),
        verification_code: format_verification_code(code),
        continue_line: "If the wallet page shows that same code, tap Continue.",
        return_line:
            "After passkey or wallet creation, return here. If needed, one more authorization link may still be required before this host is ready.",
    }
}

fn format_verification_code(code: &str) -> String {
    if code.len() == 8 {
        format!("{}-{}", &code[..4], &code[4..])
    } else {
        code.to_string()
    }
}

fn set_auth_query_param(url: &mut Url, key: &str, value: &str) {
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != key)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    url.set_query(None);
    let mut query = url.query_pairs_mut();
    for (k, v) in pairs {
        query.append_pair(&k, &v);
    }
    query.append_pair(key, value);
}

fn should_track_callback_window(status: BrowserLaunchStatus) -> bool {
    matches!(status, BrowserLaunchStatus::Opened)
}

struct AuthCallback {
    account_address: String,
    key_authorization: Option<String>,
    duration_secs: u64,
}

/// Poll the auth server until the user approves or the timeout expires.
async fn poll_until_authorized(
    client: &reqwest::Client,
    base_url: &str,
    code: &str,
    code_verifier: &str,
    network: NetworkId,
) -> Result<AuthCallback, TempoError> {
    let start = Instant::now();
    let timeout = Duration::from_secs(CALLBACK_TIMEOUT_SECS);

    loop {
        if start.elapsed() >= timeout {
            return Err(KeyError::LoginExpired.into());
        }

        let resp = poll_device_code(client, base_url, code, code_verifier, network).await?;

        if let Some(err) = &resp.error {
            if err.to_lowercase().contains("expired") {
                return Err(KeyError::LoginExpired.into());
            }
            // Intentional server-provided reason passthrough: the poll response only
            // contains a string error field (no structured source object).
            return Err(NetworkError::ResponseSchema {
                context: "login poll response",
                reason: err.clone(),
            }
            .into());
        }

        if resp.status == PollStatus::Authorized {
            return Ok(AuthCallback {
                account_address: resp
                    .account_address
                    .ok_or_else(|| TempoError::from(InputError::MissingAuthorizedAccountAddress))?,
                key_authorization: resp.key_authorization,
                duration_secs: start.elapsed().as_secs(),
            });
        }

        tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }
}

/// Save authentication keys to keys.toml (NOT in the OS keychain).
fn save_keys(
    network: NetworkId,
    keys: &Keystore,
    callback: AuthCallback,
    local_signer: PrivateKeySigner,
) -> Result<(), TempoError> {
    let wallet_address: Address =
        callback
            .account_address
            .parse()
            .map_err(|_| ConfigError::InvalidAddress {
                context: "authorized response account_address",
                value: callback.account_address.clone(),
            })?;

    let validated = tempo_common::keys::authorization::validate(
        callback.key_authorization.as_deref(),
        local_signer.address(),
    )?;
    let key_auth_hex = validated.as_ref().map(|v| v.hex.clone());

    let access_key_hex = Zeroizing::new(format!("0x{}", hex::encode(local_signer.to_bytes())));
    let access_key_address = local_signer.address();

    let mut keys = keys.clone();

    let default_chain_id = network.chain_id();
    let chain_id = validated
        .as_ref()
        .and_then(|v| (v.chain_id != 0).then_some(v.chain_id))
        .unwrap_or(default_chain_id);

    let entry = keys.upsert_by_wallet_address_and_chain(wallet_address, chain_id);

    if let Some(ref v) = validated {
        entry.key_type = v.key_type;
        entry.expiry = Some(v.expiry);
        entry.limits = v.limits.clone();
    }

    entry.wallet_type = WalletType::Passkey;
    entry.set_wallet_address(wallet_address);
    entry.set_key_address(Some(access_key_address));
    entry.key = Some(access_key_hex);
    entry.key_authorization = key_auth_hex;
    entry.provisioned = false;

    keys.save()
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum PollStatus {
    Authorized,
    #[serde(other)]
    Pending,
}

#[derive(Debug, Deserialize)]
struct PollResponse {
    status: PollStatus,
    account_address: Option<String>,
    key_authorization: Option<String>,
    error: Option<String>,
}

async fn create_device_code(
    client: &reqwest::Client,
    base_url: &str,
    pub_key: &str,
    code_challenge: &str,
    network: NetworkId,
) -> Result<String, TempoError> {
    let url = format!("{base_url}/cli-auth/device-code");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "pub_key": pub_key,
            "key_type": "secp256k1",
            "code_challenge": code_challenge,
            "network": network.auth_network_slug(),
            "chain_id": network.chain_id(),
        }))
        .send()
        .await
        .map_err(NetworkError::Reqwest)?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.map_err(NetworkError::Reqwest)?;
        return Err(NetworkError::HttpStatus {
            operation: "request device code",
            status: status.as_u16(),
            body: Some(body),
        }
        .into());
    }

    #[derive(Deserialize)]
    struct DeviceCodeResponse {
        code: String,
    }

    let body = resp.text().await.map_err(NetworkError::Reqwest)?;
    serde_json::from_str::<DeviceCodeResponse>(&body)
        .map(|r| r.code)
        .map_err(|source| NetworkError::ResponseParse {
            context: "login device code response",
            source,
        })
        .map_err(TempoError::from)
}

async fn poll_device_code(
    client: &reqwest::Client,
    base_url: &str,
    code: &str,
    code_verifier: &str,
    network: NetworkId,
) -> Result<PollResponse, TempoError> {
    let url = format!("{base_url}/cli-auth/poll/{code}");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "code_verifier": code_verifier,
            "network": network.auth_network_slug(),
            "chain_id": network.chain_id(),
        }))
        .send()
        .await
        .map_err(NetworkError::Reqwest)?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(KeyError::LoginExpired.into());
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.map_err(NetworkError::Reqwest)?;
        return Err(NetworkError::HttpStatus {
            operation: "poll login status",
            status: status.as_u16(),
            body: Some(body),
        }
        .into());
    }

    let body = resp.text().await.map_err(NetworkError::Reqwest)?;
    serde_json::from_str::<PollResponse>(&body)
        .map_err(|source| NetworkError::ResponseParse {
            context: "login poll response",
            source,
        })
        .map_err(TempoError::from)
}

fn generate_pkce_pair() -> Result<(String, String), TempoError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|source| KeyError::SigningOperationSource {
        operation: "generate PKCE verifier",
        source: Box::new(source),
    })?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

    Ok((verifier, challenge))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::auth::BrowserLaunchStatus;

    #[test]
    fn test_pkce_pair_lengths() {
        let (verifier, challenge) = generate_pkce_pair().expect("pkce generation should succeed");
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge.len(), 43);
    }

    #[test]
    fn test_pkce_pair_is_base64url() {
        let (verifier, challenge) = generate_pkce_pair().expect("pkce generation should succeed");
        let is_base64url = |s: &str| {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        };
        assert!(is_base64url(&verifier));
        assert!(is_base64url(&challenge));
    }

    #[test]
    fn test_pkce_challenge_is_deterministic() {
        let mut hasher = Sha256::new();
        hasher.update(b"deterministic-verifier");
        let c1 = URL_SAFE_NO_PAD.encode(hasher.finalize());

        let mut hasher = Sha256::new();
        hasher.update(b"deterministic-verifier");
        let c2 = URL_SAFE_NO_PAD.encode(hasher.finalize());

        assert_eq!(c1, c2);
    }

    #[test]
    fn test_pkce_pairs_are_unique() {
        let (v1, _) = generate_pkce_pair().expect("pkce generation should succeed");
        let (v2, _) = generate_pkce_pair().expect("pkce generation should succeed");
        assert_ne!(v1, v2);
    }

    #[test]
    fn callback_window_is_only_tracked_when_browser_launch_opens() {
        assert!(should_track_callback_window(BrowserLaunchStatus::Opened));
        assert!(!should_track_callback_window(BrowserLaunchStatus::Skipped));
        assert!(!should_track_callback_window(BrowserLaunchStatus::Failed));
    }

    #[test]
    fn remote_login_prompt_covers_required_remote_handoff_steps() {
        let prompt = remote_login_prompt(
            NetworkId::TempoModerato,
            "https://wallet.tempo.xyz/cli-auth?code=ANMGE375",
            "ANMGE375",
        );

        assert_eq!(prompt.network_line, "Network: testnet");
        assert_eq!(
            prompt.auth_url_line,
            "Open this link on your device: https://wallet.tempo.xyz/cli-auth?code=ANMGE375"
        );
        assert_eq!(prompt.verification_code, "ANMG-E375");
        assert_eq!(
            prompt.continue_line,
            "If the wallet page shows that same code, tap Continue."
        );
        assert!(prompt.return_line.contains("return here"));
        assert!(prompt.return_line.contains("one more authorization link"));
        assert!(prompt.return_line.contains("this host is ready"));
    }

    #[test]
    fn set_auth_query_param_replaces_existing_network() {
        let mut url = Url::parse("https://wallet.tempo.xyz/cli-auth?network=mainnet&code=OLD")
            .expect("valid url");

        set_auth_query_param(&mut url, "network", "testnet");
        set_auth_query_param(&mut url, "chain_id", "42431");
        set_auth_query_param(&mut url, "code", "ANMGE375");

        assert_eq!(
            url.as_str(),
            "https://wallet.tempo.xyz/cli-auth?network=testnet&chain_id=42431&code=ANMGE375"
        );
    }
}
