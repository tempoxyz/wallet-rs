//! Fund command — open browser deeplinks for adding funds to your Tempo wallet.

use std::time::{Duration, Instant};

use alloy::primitives::U256;
use serde::Deserialize;
use url::Url;

use crate::{
    analytics,
    analytics::{WalletFundFailurePayload, WalletFundPayload},
    wallet::{query_all_balances, TokenBalance},
};
use tempo_common::{
    cli::{context::Context, output::OutputFormat},
    error::{ConfigError, InputError, NetworkError, TempoError},
    keys::Keystore,
    security::sanitize_error,
};

/// Interval between balance poll attempts (seconds).
const POLL_INTERVAL_SECS: u64 = 3;

/// Maximum time to wait for balance change (seconds).
const CALLBACK_TIMEOUT_SECS: u64 = 900;

/// Raw contract units per user-visible credit.
const RAW_TO_CREDITS: u64 = 10_000;

#[derive(Debug)]
enum CompletionWatch {
    Token {
        wallet_address: String,
        before: Vec<TokenBalance>,
    },
    Credits {
        wallet_address: String,
        auth_server_url: String,
        before_raw: U256,
    },
}

impl CompletionWatch {
    const fn waiting_message(&self) -> &'static str {
        match self {
            Self::Token { .. } => "Waiting for funding...",
            Self::Credits { .. } => "Waiting for credits...",
        }
    }

    const fn timeout_subject(&self) -> &'static str {
        match self {
            Self::Token { .. } => "funding",
            Self::Credits { .. } => "credits",
        }
    }
}

#[derive(Debug, Deserialize)]
struct CoinflowBalancesResponse {
    credits: CoinflowBalance,
}

#[derive(Debug, Deserialize)]
struct CoinflowBalance {
    #[serde(default, rename = "rawAmount")]
    raw_amount: Option<CoinflowAmount>,
    #[serde(default)]
    cents: Option<CoinflowAmount>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CoinflowAmount {
    Number(u64),
    String(String),
}

impl CoinflowAmount {
    fn as_u256(&self, field: &'static str) -> Result<U256, NetworkError> {
        match self {
            Self::Number(value) => Ok(U256::from(*value)),
            Self::String(value) => {
                value
                    .trim()
                    .parse::<U256>()
                    .map_err(|_| NetworkError::ResponseSchema {
                        context: "coinflow balances response",
                        reason: format!("invalid {field}: {value}"),
                    })
            }
        }
    }
}

impl CoinflowBalance {
    fn raw_balance(&self) -> Result<U256, NetworkError> {
        if let Some(raw_amount) = &self.raw_amount {
            return raw_amount.as_u256("credits.rawAmount");
        }

        if let Some(cents) = &self.cents {
            return Ok(cents.as_u256("credits.cents")? * U256::from(RAW_TO_CREDITS / 100));
        }

        Err(NetworkError::ResponseMissingField {
            context: "coinflow balances response",
            field: "credits.rawAmount or credits.cents",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Fund,
    Crypto,
    Credits,
    ReferralCode(String),
}

impl Target {
    pub(crate) fn from_cli(crypto: bool, credits: bool, referral_code: Option<String>) -> Self {
        if let Some(code) = referral_code {
            Self::ReferralCode(code)
        } else if credits {
            Self::Credits
        } else if crypto {
            Self::Crypto
        } else {
            Self::Fund
        }
    }

    const fn analytics_target(&self) -> &'static str {
        match self {
            Self::Fund => "fund",
            Self::Crypto => "crypto",
            Self::Credits => "credits",
            Self::ReferralCode(_) => "referral",
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub(crate) async fn run(
    ctx: &Context,
    address: Option<String>,
    no_browser: bool,
    target: Target,
) -> Result<(), TempoError> {
    let method = fund_method(no_browser);
    track_fund_start(ctx, method, target.analytics_target());
    let result = run_inner(ctx, address, no_browser, &target).await;
    track_fund_result(ctx, method, target.analytics_target(), &result);
    result
}

async fn run_inner(
    ctx: &Context,
    address: Option<String>,
    no_browser: bool,
    target: &Target,
) -> Result<(), TempoError> {
    let auth_server_url =
        std::env::var("TEMPO_AUTH_URL").unwrap_or_else(|_| ctx.network.auth_url().to_string());
    let completion_watch = prepare_completion_watch(ctx, address, target, &auth_server_url).await?;
    let fund_url = build_fund_url(&auth_server_url, target)?;
    let show_status = no_browser || ctx.output_format == OutputFormat::Text;

    if show_status {
        eprintln!("Fund URL: {fund_url}");
    }

    super::auth::try_open_browser(&fund_url, no_browser);

    if no_browser {
        show_remote_fund_prompt(target, &fund_url);
    }

    if show_status {
        eprintln!("{}", completion_watch.waiting_message());
    }

    wait_for_completion(ctx, &completion_watch, show_status).await
}

async fn prepare_completion_watch(
    ctx: &Context,
    address: Option<String>,
    target: &Target,
    auth_server_url: &str,
) -> Result<CompletionWatch, TempoError> {
    match target {
        Target::Credits => {
            let wallet_address = resolve_address(address, &ctx.keys)?;
            let before_raw = query_credit_balance(auth_server_url, &wallet_address).await?;

            Ok(CompletionWatch::Credits {
                wallet_address,
                auth_server_url: auth_server_url.to_string(),
                before_raw,
            })
        }
        Target::Fund | Target::Crypto | Target::ReferralCode(_) => {
            let wallet_address = resolve_address(address, &ctx.keys)?;
            let before = query_all_balances(&ctx.config, ctx.network, &wallet_address).await;

            Ok(CompletionWatch::Token {
                wallet_address,
                before,
            })
        }
    }
}

async fn wait_for_completion(
    ctx: &Context,
    completion_watch: &CompletionWatch,
    show_status: bool,
) -> Result<(), TempoError> {
    let start = Instant::now();
    let timeout = Duration::from_secs(CALLBACK_TIMEOUT_SECS);
    let interval = Duration::from_secs(POLL_INTERVAL_SECS);

    loop {
        if start.elapsed() >= timeout {
            if show_status {
                eprintln!(
                    "Timed out waiting for {} after {} minutes.",
                    completion_watch.timeout_subject(),
                    CALLBACK_TIMEOUT_SECS / 60
                );
            }
            return Ok(());
        }

        tokio::time::sleep(interval).await;

        match completion_watch {
            CompletionWatch::Token {
                wallet_address,
                before,
            } => {
                let current = query_all_balances(&ctx.config, ctx.network, wallet_address).await;

                if has_balance_changed(before, &current) {
                    if show_status {
                        eprintln!("\nFunding received!");
                        render_balance_diff(before, &current);
                    }
                    return Ok(());
                }
            }
            CompletionWatch::Credits {
                wallet_address,
                auth_server_url,
                before_raw,
            } => {
                let current_raw = query_credit_balance(auth_server_url, wallet_address).await?;

                if current_raw > *before_raw {
                    if show_status {
                        eprintln!("\nCredits received!");
                        render_credit_balance_diff(*before_raw, current_raw);
                    }
                    return Ok(());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the target wallet address from an explicit arg or the keystore default.
pub(crate) fn resolve_address(
    address: Option<String>,
    keys: &Keystore,
) -> Result<String, TempoError> {
    if let Some(addr) = address {
        let parsed = tempo_common::security::parse_address_input(&addr, "wallet address")?;
        return Ok(format!("{parsed:#x}"));
    }

    keys.wallet_address_hex().ok_or_else(|| {
        ConfigError::Missing("No wallet configured. Run 'tempo wallet login'.".to_string()).into()
    })
}

fn build_fund_url(auth_server_url: &str, target: &Target) -> Result<String, TempoError> {
    let mut url = Url::parse(auth_server_url).map_err(|source| InputError::UrlParseFor {
        context: "auth server",
        source,
    })?;

    url.set_path("/");
    url.set_query(None);

    {
        let mut query = url.query_pairs_mut();
        match target {
            Target::Fund => {
                query.append_pair("action", "fund");
            }
            Target::Crypto => {
                query.append_pair("action", "crypto");
            }
            Target::Credits => {
                query.append_pair("action", "credits");
            }
            Target::ReferralCode(code) => {
                query.append_pair("claim", code);
            }
        }
    }

    Ok(url.to_string())
}

fn build_coinflow_balances_url(
    auth_server_url: &str,
    wallet_address: &str,
) -> Result<String, TempoError> {
    let mut url = Url::parse(auth_server_url).map_err(|source| InputError::UrlParseFor {
        context: "auth server",
        source,
    })?;

    url.set_path("/api/coinflow/balances");
    url.set_query(None);
    url.query_pairs_mut().append_pair("wallet", wallet_address);

    Ok(url.to_string())
}

pub(crate) async fn query_credit_balance(
    auth_server_url: &str,
    wallet_address: &str,
) -> Result<U256, TempoError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("tempo-wallet/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(NetworkError::Reqwest)?;
    let url = build_coinflow_balances_url(auth_server_url, wallet_address)?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(NetworkError::Reqwest)?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.ok();
        return Err(NetworkError::HttpStatus {
            operation: "fetch coinflow balances",
            status: status.as_u16(),
            body,
        }
        .into());
    }

    let body = resp.text().await.map_err(NetworkError::Reqwest)?;
    let balances: CoinflowBalancesResponse =
        serde_json::from_str(&body).map_err(|source| NetworkError::ResponseParse {
            context: "coinflow balances response",
            source,
        })?;

    Ok(balances.credits.raw_balance()?)
}

/// Returns `true` if any token balance differs between `initial` and `current`.
fn has_balance_changed(initial: &[TokenBalance], current: &[TokenBalance]) -> bool {
    if current.len() != initial.len() {
        return true;
    }
    for cur in current {
        let prev = initial.iter().find(|b| b.token == cur.token);
        match prev {
            Some(prev) if prev.balance != cur.balance => return true,
            None => return true,
            _ => {}
        }
    }
    false
}

/// Render per-token balance changes to stderr.
fn render_balance_diff(before: &[TokenBalance], after: &[TokenBalance]) {
    for cur in after {
        let prev = before
            .iter()
            .find(|b| b.token == cur.token)
            .map_or("0", |b| b.balance.as_str());
        if cur.balance != prev {
            eprintln!("  {} balance: {} -> {}", cur.symbol, prev, cur.balance);
        }
    }
}

fn render_credit_balance_diff(before_raw: U256, after_raw: U256) {
    eprintln!(
        "  Credit balance: {} -> {}",
        format_credit_balance(before_raw),
        format_credit_balance(after_raw)
    );
}

pub(crate) fn format_credit_balance(raw: U256) -> String {
    let divisor = U256::from(RAW_TO_CREDITS);
    let whole = raw / divisor;
    let fractional = u64::try_from(raw % divisor).expect("fractional credits fit in u64");

    if fractional == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{fractional:04}")
    }
}

fn fund_method(no_browser: bool) -> &'static str {
    if no_browser {
        "manual"
    } else {
        "browser"
    }
}

fn show_remote_fund_prompt(target: &Target, fund_url: &str) {
    eprintln!("Open this link on your device: {fund_url}");
    match target {
        Target::Credits => {
            eprintln!("Complete the credits purchase in the wallet app.");
            eprintln!("After purchasing credits, return here to continue.");
        }
        Target::Fund | Target::Crypto | Target::ReferralCode(_) => {
            eprintln!("After funding is complete, return here to continue.");
        }
    }
}

// ---------------------------------------------------------------------------
// Analytics
// ---------------------------------------------------------------------------

fn track_fund_start(ctx: &Context, method: &str, target: &str) {
    ctx.track(
        analytics::WALLET_FUND_STARTED,
        WalletFundPayload {
            network: ctx.network.as_str().to_string(),
            method: method.to_string(),
            target: target.to_string(),
        },
    );
}

fn track_fund_result(ctx: &Context, method: &str, target: &str, result: &Result<(), TempoError>) {
    match result {
        Ok(()) => {
            ctx.track(
                analytics::WALLET_FUND_SUCCESS,
                WalletFundPayload {
                    network: ctx.network.as_str().to_string(),
                    method: method.to_string(),
                    target: target.to_string(),
                },
            );
        }
        Err(e) => {
            ctx.track(
                analytics::WALLET_FUND_FAILURE,
                WalletFundFailurePayload {
                    network: ctx.network.as_str().to_string(),
                    method: method.to_string(),
                    target: target.to_string(),
                    error: sanitize_error(&e.to_string()),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_coinflow_balances_url, build_fund_url, fund_method, Target};

    #[test]
    fn fund_method_uses_manual_only_when_no_browser_is_true() {
        assert_eq!(fund_method(true), "manual");
        assert_eq!(fund_method(false), "browser");
    }

    #[test]
    fn build_fund_url_uses_expected_query_for_each_target() {
        assert_eq!(
            build_fund_url("https://wallet.moderato.tempo.xyz/cli-auth", &Target::Fund).unwrap(),
            "https://wallet.moderato.tempo.xyz/?action=fund"
        );
        assert_eq!(
            build_fund_url(
                "https://wallet.moderato.tempo.xyz/cli-auth",
                &Target::Crypto
            )
            .unwrap(),
            "https://wallet.moderato.tempo.xyz/?action=crypto"
        );
        assert_eq!(
            build_fund_url(
                "https://wallet.moderato.tempo.xyz/cli-auth",
                &Target::Credits
            )
            .unwrap(),
            "https://wallet.moderato.tempo.xyz/?action=credits"
        );
        assert_eq!(
            build_fund_url(
                "https://wallet.moderato.tempo.xyz/cli-auth",
                &Target::ReferralCode("ABC123".to_string())
            )
            .unwrap(),
            "https://wallet.moderato.tempo.xyz/?claim=ABC123"
        );
    }

    #[test]
    fn build_coinflow_balances_url_uses_wallet_root() {
        assert_eq!(
            build_coinflow_balances_url("https://wallet.moderato.tempo.xyz/cli-auth", "0x1234")
                .unwrap(),
            "https://wallet.moderato.tempo.xyz/api/coinflow/balances?wallet=0x1234"
        );
    }
}
