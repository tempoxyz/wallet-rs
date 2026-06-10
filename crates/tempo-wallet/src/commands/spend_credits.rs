//! Spend credits via Coinflow redeem flow.

use std::{fs, io::Write, path::Path, time::Duration};

use alloy::{
    primitives::{address, keccak256, Address, Bytes, TxKind, B256, U256},
    providers::Provider,
    sol,
    sol_types::SolCall,
};
use mpp::{client::tempo::signing::TempoSigningMode, tempo::TempoChargeExt};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::commands::fund;
use tempo_common::{
    cli::{context::Context, output, output::OutputFormat},
    error::{ConfigError, InputError, NetworkError, TempoError},
    keys::Signer,
    payment::session::submit_tempo_tx,
};

const COINFLOW_AUTH_SUBTOTAL_RETRY_BUFFER_CENTS: u64 = 1;
const ACCOUNT_KEYCHAIN_ADDRESS: Address = address!("aaaaaaaa00000000000000000000000000000000");
const KEY_AUTH_POLL_ATTEMPTS: usize = 20;
const KEY_AUTH_POLL_INTERVAL: Duration = Duration::from_secs(1);

sol! {
    interface ITIP20Credits {
        function transferWithMemo(address to, uint256 amount, bytes32 memo) external returns (bool);
    }
}

sol! {
    #[sol(rpc)]
    interface IAccountKeychain {
        struct KeyInfo {
            uint8 signatureType;
            address keyId;
            uint64 expiry;
            bool enforceLimits;
            bool isRevoked;
        }

        function getKey(address account, address publicKey) external view returns (KeyInfo memory);
    }
}

#[derive(Debug, Deserialize)]
struct AuthMsgResponse {
    message: String,
    #[serde(rename = "validBefore")]
    valid_before: String,
    nonce: String,
    #[serde(rename = "creditsRawAmount")]
    credits_raw_amount: u64,
}

#[derive(Debug, Deserialize)]
struct RedeemResponse {
    hash: String,
}

#[derive(Debug, Serialize)]
struct SpendCreditsResult {
    wallet: String,
    amount_cents: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    dry_run: bool,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

struct SubmitRedeemParams<'a> {
    base_url: &'a str,
    wallet: &'a str,
    amount_cents: u64,
    transaction_data: &'a serde_json::Value,
    auth_resp: &'a AuthMsgResponse,
    signature: &'a str,
    output_format: OutputFormat,
}

pub(crate) async fn run(
    ctx: &Context,
    amount_cents: u64,
    to: String,
    data: String,
    value: String,
    address: Option<String>,
    dry_run: bool,
) -> Result<(), TempoError> {
    let transaction_data = build_transaction_data(&to, &data, &value)?;

    run_with_transaction_data(ctx, amount_cents, transaction_data, address, dry_run).await
}

pub(crate) async fn run_mpp(
    ctx: &Context,
    challenge: String,
    client_id: Option<String>,
    address: Option<String>,
    dry_run: bool,
) -> Result<(), TempoError> {
    let challenge = parse_mpp_challenge(&challenge)?;
    let mpp_transaction = build_mpp_transaction(&challenge, client_id.as_deref(), ctx)?;

    if ctx.output_format == OutputFormat::Text {
        eprintln!(
            "Preparing MPP credits payment: {} cents to {} via {}",
            mpp_transaction.amount_cents, mpp_transaction.recipient, mpp_transaction.token
        );
    }

    run_with_transaction_data(
        ctx,
        mpp_transaction.amount_cents,
        mpp_transaction.transaction_data,
        address,
        dry_run,
    )
    .await
}

pub(crate) async fn run_mpp_file(
    ctx: &Context,
    challenge_path: &Path,
    client_id: Option<String>,
    address: Option<String>,
    dry_run: bool,
) -> Result<(), TempoError> {
    let challenge = fs::read_to_string(challenge_path).map_err(|source| InputError::ReadFile {
        path: challenge_path.display().to_string(),
        source,
    })?;
    run_mpp(ctx, challenge, client_id, address, dry_run).await
}

async fn run_with_transaction_data(
    ctx: &Context,
    amount_cents: u64,
    transaction_data: serde_json::Value,
    address: Option<String>,
    dry_run: bool,
) -> Result<(), TempoError> {
    let auth_server_url =
        std::env::var("TEMPO_AUTH_URL").unwrap_or_else(|_| ctx.network.auth_url().to_string());
    let wallet = fund::resolve_address(address, &ctx.keys)?;
    let wallet_address = tempo_common::security::parse_address_input(&wallet, "wallet address")?;

    if dry_run {
        if ctx.output_format == OutputFormat::Text {
            eprintln!("[DRY RUN] Credits redeem transaction ready, skipping authorization and submission.");
        }
        return SpendCreditsResult {
            wallet,
            amount_cents,
            tx_hash: None,
            dry_run: true,
        }
        .render(ctx.output_format);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("tempo-wallet/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(NetworkError::Reqwest)?;

    let base_url = build_api_base_url(&auth_server_url)?;
    let rpc_url = ctx.config.rpc_url(ctx.network);
    let provider = alloy::providers::RootProvider::<mpp::client::TempoNetwork>::new_http(rpc_url);

    let mut auth_subtotal_cents = amount_cents;
    let redeem_resp = loop {
        let signer_info = ctx
            .keys
            .signer_for_wallet_address(wallet_address, ctx.network)?;

        ensure_access_key_authorized(ctx, &provider, &signer_info).await?;

        let auth_resp = request_credits_auth_message(
            &client,
            &base_url,
            &wallet,
            auth_subtotal_cents,
            &transaction_data,
            ctx.output_format,
        )
        .await?;

        if ctx.output_format == OutputFormat::Text {
            eprintln!("Signing authorization...");
        }

        let eip712_digest = compute_eip712_signing_hash(&auth_resp.message)?;
        let signature =
            signer_info.sign_hash_hex(&eip712_digest, "sign EIP-712 credits authorization")?;

        match submit_redeem_transaction(
            &client,
            SubmitRedeemParams {
                base_url: &base_url,
                wallet: &wallet,
                amount_cents,
                transaction_data: &transaction_data,
                auth_resp: &auth_resp,
                signature: &signature,
                output_format: ctx.output_format,
            },
        )
        .await
        {
            Ok(response) => break response,
            Err(NetworkError::HttpStatus { body, .. })
                if auth_subtotal_cents == amount_cents
                    && body
                        .as_deref()
                        .is_some_and(is_max_credits_authorized_mismatch) =>
            {
                auth_subtotal_cents =
                    amount_cents.saturating_add(COINFLOW_AUTH_SUBTOTAL_RETRY_BUFFER_CENTS);
                if ctx.output_format == OutputFormat::Text {
                    eprintln!(
                        "Coinflow fee estimate changed between authorization and submit; retrying with refreshed authorization..."
                    );
                }
            }
            Err(error) => return Err(error.into()),
        }
    };

    let tx_hash = redeem_resp.hash;

    let result = SpendCreditsResult {
        wallet,
        amount_cents,
        tx_hash: Some(tx_hash),
        dry_run: false,
    };

    result.render(ctx.output_format)
}

#[derive(Debug)]
struct MppCreditsTransaction {
    amount_cents: u64,
    token: String,
    recipient: String,
    transaction_data: serde_json::Value,
}

fn parse_mpp_challenge(input: &str) -> Result<mpp::PaymentChallenge, TempoError> {
    let trimmed = input.trim();
    let challenge = trimmed
        .lines()
        .find_map(|line| {
            let line = line.trim();
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("www-authenticate")
                    .then_some(value.trim())
            })
        })
        .unwrap_or(trimmed);

    if challenge.starts_with('{') {
        return serde_json::from_str(challenge)
            .map_err(|source| NetworkError::ResponseParse {
                context: "MPP challenge JSON",
                source,
            })
            .map_err(Into::into);
    }

    mpp::PaymentChallenge::from_header(challenge)
        .map_err(|error| ConfigError::Invalid(format!("invalid MPP challenge: {error}")).into())
}

fn build_mpp_transaction(
    challenge: &mpp::PaymentChallenge,
    client_id: Option<&str>,
    ctx: &Context,
) -> Result<MppCreditsTransaction, TempoError> {
    if challenge.method.as_str() != "tempo" {
        return Err(ConfigError::Invalid(format!(
            "unsupported MPP method for Coinflow credits: {}",
            challenge.method.as_str()
        ))
        .into());
    }
    if challenge.intent.as_str() != "charge" {
        return Err(ConfigError::Invalid(format!(
            "unsupported MPP intent for Coinflow credits: {}",
            challenge.intent.as_str()
        ))
        .into());
    }
    if challenge.is_expired() {
        return Err(ConfigError::Invalid("MPP challenge is expired".to_string()).into());
    }

    let charge: mpp::ChargeRequest = challenge
        .request
        .decode()
        .map_err(|error| ConfigError::Invalid(format!("invalid MPP charge request: {error}")))?;
    if let Some(chain_id) = charge.chain_id() {
        if chain_id != ctx.network.chain_id() {
            return Err(ConfigError::Invalid(format!(
                "MPP challenge is for chain {chain_id}, but the selected network is {} (chain {})",
                ctx.network,
                ctx.network.chain_id()
            ))
            .into());
        }
    }
    if charge
        .splits()
        .map_err(|error| ConfigError::Invalid(format!("invalid MPP splits: {error}")))?
        .is_some_and(|splits| !splits.is_empty())
    {
        return Err(ConfigError::Invalid(
            "MPP split payments are not supported with Coinflow credits yet".to_string(),
        )
        .into());
    }

    let token_config = ctx.network.token();
    let token = tempo_common::security::parse_address_input(&charge.currency, "MPP currency")?;
    if token != token_config.address {
        return Err(ConfigError::Invalid(format!(
            "MPP challenge currency {token:#x} does not match {} on {} ({:#x})",
            token_config.symbol, ctx.network, token_config.address
        ))
        .into());
    }

    let recipient = charge.recipient.as_deref().ok_or_else(|| {
        ConfigError::Invalid("MPP challenge is missing a recipient address".to_string())
    })?;
    let recipient_address =
        tempo_common::security::parse_address_input(recipient, "MPP recipient")?;
    let amount_atomic = charge
        .parse_amount()
        .map_err(|error| ConfigError::Invalid(format!("invalid MPP amount: {error}")))?;
    let amount = U256::from(amount_atomic);
    let amount_cents = amount_to_usd_cents(amount_atomic, token_config.decimals)?;
    let memo = match charge.memo() {
        Some(memo) => parse_memo_hex(&memo)?,
        None => mpp::tempo::attribution::encode(&challenge.id, &challenge.realm, client_id),
    };
    let data = Bytes::from(
        ITIP20Credits::transferWithMemoCall {
            to: recipient_address,
            amount,
            memo: B256::from(memo),
        }
        .abi_encode(),
    );

    Ok(MppCreditsTransaction {
        amount_cents,
        token: format!("{token:#x}"),
        recipient: format!("{recipient_address:#x}"),
        transaction_data: build_transaction_data(
            &format!("{token:#x}"),
            &format!("{data:#x}"),
            "0",
        )?,
    })
}

fn amount_to_usd_cents(amount_atomic: u128, decimals: u8) -> Result<u64, TempoError> {
    if decimals < 2 {
        return Err(ConfigError::Invalid(format!(
            "cannot convert token with {decimals} decimals to USD cents"
        ))
        .into());
    }
    let base_units_per_cent = 10u128.pow(u32::from(decimals - 2));
    if amount_atomic == 0 {
        return Err(ConfigError::Invalid(
            "MPP challenge amount must be greater than zero".to_string(),
        )
        .into());
    }
    if !amount_atomic.is_multiple_of(base_units_per_cent) {
        return Err(ConfigError::Invalid(format!(
            "MPP challenge amount {amount_atomic} cannot be represented exactly in Coinflow credits cents for a {decimals}-decimal token"
        ))
        .into());
    }

    u64::try_from(amount_atomic / base_units_per_cent).map_err(|_| {
        ConfigError::Invalid(format!(
            "MPP challenge amount {amount_atomic} is too large for Coinflow credits"
        ))
        .into()
    })
}

fn parse_memo_hex(memo: &str) -> Result<[u8; 32], TempoError> {
    let memo_hex = memo.strip_prefix("0x").unwrap_or(memo);
    let bytes = hex::decode(memo_hex)
        .map_err(|_| InputError::InvalidHexInput(format!("invalid MPP memo: {memo}")))?;
    if bytes.len() != 32 {
        return Err(InputError::InvalidHexInput(format!(
            "invalid MPP memo length: expected 32 bytes, got {}",
            bytes.len()
        ))
        .into());
    }

    let mut memo_bytes = [0u8; 32];
    memo_bytes.copy_from_slice(&bytes);
    Ok(memo_bytes)
}

async fn ensure_access_key_authorized(
    ctx: &Context,
    provider: &alloy::providers::RootProvider<mpp::client::TempoNetwork>,
    signer: &Signer,
) -> Result<(), TempoError> {
    let (wallet, access_key) = match &signer.signing_mode {
        TempoSigningMode::Direct => return Ok(()),
        TempoSigningMode::Keychain { wallet, .. } => (*wallet, signer.signer.address()),
    };

    if is_access_key_authorized(provider, wallet, access_key).await? {
        return Ok(());
    }

    let provisioning_signer = signer.with_key_authorization().ok_or_else(|| {
        TempoError::from(ConfigError::Missing(format!(
            "Access key {access_key:#x} is not authorized for wallet {wallet:#x}, and no stored key_authorization is available. Run `tempo wallet login` again to approve this access key."
        )))
    })?;

    if ctx.output_format == OutputFormat::Text {
        eprintln!("Authorizing access key...");
    }

    let tx_hash = submit_tempo_tx(
        provider,
        &provisioning_signer,
        ctx.network.chain_id(),
        ctx.network.token().address,
        wallet,
        vec![tempo_primitives::transaction::Call {
            to: TxKind::Call(wallet),
            value: U256::ZERO,
            input: Bytes::default(),
        }],
    )
    .await?;

    wait_for_access_key_authorized(provider, wallet, access_key, &tx_hash).await?;

    if ctx.output_format == OutputFormat::Text {
        eprintln!("Access key authorized.");
    }

    Ok(())
}

async fn wait_for_access_key_authorized(
    provider: &impl Provider<mpp::client::TempoNetwork>,
    wallet: Address,
    access_key: Address,
    tx_hash: &str,
) -> Result<(), TempoError> {
    for _ in 0..KEY_AUTH_POLL_ATTEMPTS {
        if is_access_key_authorized(provider, wallet, access_key).await? {
            return Ok(());
        }
        sleep(KEY_AUTH_POLL_INTERVAL).await;
    }

    Err(NetworkError::Rpc {
        operation: "authorize access key",
        reason: format!(
            "timed out waiting for key authorization transaction {tx_hash} to register access key {access_key:#x} for wallet {wallet:#x}"
        ),
    }
    .into())
}

async fn is_access_key_authorized(
    provider: &impl Provider<mpp::client::TempoNetwork>,
    wallet: Address,
    access_key: Address,
) -> Result<bool, TempoError> {
    let contract = IAccountKeychain::new(ACCOUNT_KEYCHAIN_ADDRESS, provider);
    let key = contract
        .getKey(wallet, access_key)
        .call()
        .await
        .map_err(|source| NetworkError::RpcSource {
            operation: "query access key authorization",
            source: Box::new(source),
        })?;

    Ok(key.keyId == access_key && !key.isRevoked)
}

async fn request_credits_auth_message(
    client: &reqwest::Client,
    base_url: &str,
    wallet: &str,
    auth_subtotal_cents: u64,
    transaction_data: &serde_json::Value,
    output_format: OutputFormat,
) -> Result<AuthMsgResponse, TempoError> {
    if output_format == OutputFormat::Text {
        eprintln!("Requesting credits authorization...");
    }

    let auth_msg_url = format!("{base_url}/api/coinflow/redeem/auth-msg");
    let auth_msg_body = serde_json::json!({
        "wallet": wallet,
        "subtotal": {
            "cents": auth_subtotal_cents,
            "currency": "USD"
        },
        "transactionData": transaction_data
    });

    let resp = client
        .post(&auth_msg_url)
        .json(&auth_msg_body)
        .send()
        .await
        .map_err(NetworkError::Reqwest)?;

    let resp_status = resp.status();
    let resp_text = resp.text().await.map_err(NetworkError::Reqwest)?;

    if !resp_status.is_success() {
        return Err(NetworkError::HttpStatus {
            operation: "get credits auth message",
            status: resp_status.as_u16(),
            body: Some(resp_text),
        }
        .into());
    }

    serde_json::from_str(&resp_text)
        .map_err(|source| NetworkError::ResponseParse {
            context: "auth msg",
            source,
        })
        .map_err(Into::into)
}

async fn submit_redeem_transaction(
    client: &reqwest::Client,
    params: SubmitRedeemParams<'_>,
) -> Result<RedeemResponse, NetworkError> {
    if params.output_format == OutputFormat::Text {
        eprintln!("Submitting redeem transaction...");
    }

    let redeem_url = format!("{}/api/coinflow/redeem/send", params.base_url);
    let redeem_body = serde_json::json!({
        "wallet": params.wallet,
        "subtotal": {
            "cents": params.amount_cents,
            "currency": "USD"
        },
        "transactionData": params.transaction_data,
        "permitCreditsSignature": params.signature,
        "validBefore": params.auth_resp.valid_before,
        "nonce": params.auth_resp.nonce,
        "creditsRawAmount": params.auth_resp.credits_raw_amount
    });

    let resp = client
        .post(&redeem_url)
        .json(&redeem_body)
        .send()
        .await
        .map_err(NetworkError::Reqwest)?;

    let resp_status = resp.status();
    let resp_text = resp.text().await.map_err(NetworkError::Reqwest)?;

    if !resp_status.is_success() {
        return Err(NetworkError::HttpStatus {
            operation: "send redeem transaction",
            status: resp_status.as_u16(),
            body: Some(resp_text),
        });
    }

    serde_json::from_str(&resp_text).map_err(|source| NetworkError::ResponseParse {
        context: "redeem response",
        source,
    })
}

fn is_max_credits_authorized_mismatch(body: &str) -> bool {
    body.contains("exceeds max credits authorized")
}

fn compute_eip712_signing_hash(message_json: &str) -> Result<B256, TempoError> {
    let typed_data: serde_json::Value =
        serde_json::from_str(message_json).map_err(|source| NetworkError::ResponseParse {
            context: "EIP-712 typed data",
            source,
        })?;

    let domain = &typed_data["domain"];
    let domain_separator = compute_domain_separator(domain)?;

    let primary_type = typed_data["primaryType"]
        .as_str()
        .ok_or_else(|| InputError::InvalidHexInput("missing primaryType".to_string()))?;
    let types = &typed_data["types"];
    let message = &typed_data["message"];
    let struct_hash = compute_struct_hash(primary_type, types, message)?;

    // EIP-712 digest: keccak256("\x19\x01" || domainSeparator || structHash)
    let mut digest_input = Vec::with_capacity(66);
    digest_input.extend_from_slice(&[0x19, 0x01]);
    digest_input.extend_from_slice(domain_separator.as_slice());
    digest_input.extend_from_slice(struct_hash.as_slice());
    Ok(keccak256(&digest_input))
}

/// Compute the EIP-712 domain separator hash.
fn compute_domain_separator(domain: &serde_json::Value) -> Result<B256, TempoError> {
    let mut domain_type_parts = vec![];
    let mut domain_values: Vec<Vec<u8>> = vec![];

    if domain.get("name").is_some() {
        domain_type_parts.push("string name");
        let name = domain["name"].as_str().unwrap_or("");
        domain_values.push(keccak256(name.as_bytes()).to_vec());
    }
    if domain.get("version").is_some() {
        domain_type_parts.push("string version");
        let version = domain["version"].as_str().unwrap_or("");
        domain_values.push(keccak256(version.as_bytes()).to_vec());
    }
    if domain.get("chainId").is_some() {
        domain_type_parts.push("uint256 chainId");
        let chain_id = domain["chainId"]
            .as_u64()
            .ok_or_else(|| InputError::InvalidHexInput("invalid chainId".to_string()))?;
        let mut buf = [0u8; 32];
        buf[24..].copy_from_slice(&chain_id.to_be_bytes());
        domain_values.push(buf.to_vec());
    }
    if domain.get("verifyingContract").is_some() {
        domain_type_parts.push("address verifyingContract");
        let addr_str = domain["verifyingContract"]
            .as_str()
            .ok_or_else(|| InputError::InvalidHexInput("invalid verifyingContract".to_string()))?;
        let addr: Address = addr_str.parse().map_err(|_| ConfigError::InvalidAddress {
            context: "EIP-712 domain verifyingContract",
            value: addr_str.to_string(),
        })?;
        let mut buf = [0u8; 32];
        buf[12..].copy_from_slice(addr.as_slice());
        domain_values.push(buf.to_vec());
    }
    if domain.get("salt").is_some() {
        domain_type_parts.push("bytes32 salt");
        let salt_str = domain["salt"]
            .as_str()
            .ok_or_else(|| InputError::InvalidHexInput("invalid salt".to_string()))?;
        let salt_hex = salt_str.strip_prefix("0x").unwrap_or(salt_str);
        let salt_bytes = hex::decode(salt_hex)
            .map_err(|_| InputError::InvalidHexInput("invalid salt hex".to_string()))?;
        domain_values.push(salt_bytes);
    }

    let domain_type_str = format!("EIP712Domain({})", domain_type_parts.join(","));
    let type_hash = keccak256(domain_type_str.as_bytes());

    let mut encoded = Vec::new();
    encoded.extend_from_slice(type_hash.as_slice());
    for val in &domain_values {
        encoded.extend_from_slice(val);
    }

    Ok(keccak256(&encoded))
}

/// Compute the struct hash for a given type, following EIP-712 encoding rules.
fn compute_struct_hash(
    type_name: &str,
    types: &serde_json::Value,
    data: &serde_json::Value,
) -> Result<B256, TempoError> {
    let type_hash = compute_type_hash(type_name, types)?;
    let encoded_data = encode_data(type_name, types, data)?;

    let mut full = Vec::new();
    full.extend_from_slice(type_hash.as_slice());
    full.extend_from_slice(&encoded_data);

    Ok(keccak256(&full))
}

fn compute_type_hash(type_name: &str, types: &serde_json::Value) -> Result<B256, TempoError> {
    let type_str = encode_type(type_name, types)?;
    Ok(keccak256(type_str.as_bytes()))
}

/// Encode a type string including all referenced sub-types (sorted).
fn encode_type(type_name: &str, types: &serde_json::Value) -> Result<String, TempoError> {
    let fields = types[type_name].as_array().ok_or_else(|| {
        InputError::InvalidHexInput(format!("missing type definition for {type_name}"))
    })?;

    let mut params = Vec::new();
    let mut referenced_types = std::collections::BTreeSet::new();

    for field in fields {
        let field_type = field["type"]
            .as_str()
            .ok_or_else(|| InputError::InvalidHexInput("missing field type".to_string()))?;
        let field_name = field["name"]
            .as_str()
            .ok_or_else(|| InputError::InvalidHexInput("missing field name".to_string()))?;
        params.push(format!("{field_type} {field_name}"));

        let base_type = field_type.trim_end_matches("[]");
        if types.get(base_type).is_some() && base_type != type_name {
            collect_referenced_types(base_type, types, &mut referenced_types);
        }
    }

    let primary = format!("{type_name}({})", params.join(","));
    let mut result = primary;
    for ref_type in &referenced_types {
        result.push_str(&encode_type_single(ref_type, types)?);
    }
    Ok(result)
}

fn encode_type_single(type_name: &str, types: &serde_json::Value) -> Result<String, TempoError> {
    let fields = types[type_name].as_array().ok_or_else(|| {
        InputError::InvalidHexInput(format!("missing type definition for {type_name}"))
    })?;
    let mut params = Vec::new();
    for field in fields {
        let field_type = field["type"].as_str().unwrap_or("");
        let field_name = field["name"].as_str().unwrap_or("");
        params.push(format!("{field_type} {field_name}"));
    }
    Ok(format!("{type_name}({})", params.join(",")))
}

fn collect_referenced_types(
    type_name: &str,
    types: &serde_json::Value,
    collected: &mut std::collections::BTreeSet<String>,
) {
    if !collected.insert(type_name.to_string()) {
        return;
    }
    if let Some(fields) = types[type_name].as_array() {
        for field in fields {
            if let Some(field_type) = field["type"].as_str() {
                let base_type = field_type.trim_end_matches("[]");
                if types.get(base_type).is_some() && base_type != type_name {
                    collect_referenced_types(base_type, types, collected);
                }
            }
        }
    }
}

/// Encode the data values according to EIP-712 rules.
fn encode_data(
    type_name: &str,
    types: &serde_json::Value,
    data: &serde_json::Value,
) -> Result<Vec<u8>, TempoError> {
    let fields = types[type_name].as_array().ok_or_else(|| {
        InputError::InvalidHexInput(format!("missing type definition for {type_name}"))
    })?;

    let mut encoded = Vec::new();

    for field in fields {
        let field_type = field["type"]
            .as_str()
            .ok_or_else(|| InputError::InvalidHexInput("missing field type".to_string()))?;
        let field_name = field["name"]
            .as_str()
            .ok_or_else(|| InputError::InvalidHexInput("missing field name".to_string()))?;
        let value = &data[field_name];

        let encoded_value = encode_value(field_type, types, value)?;
        encoded.extend_from_slice(&encoded_value);
    }

    Ok(encoded)
}

/// Encode a single value according to its EIP-712 type.
fn encode_value(
    field_type: &str,
    types: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<Vec<u8>, TempoError> {
    // Handle array types
    if let Some(base_type) = field_type.strip_suffix("[]") {
        let items = value
            .as_array()
            .ok_or_else(|| InputError::InvalidHexInput("expected array value".to_string()))?;
        let mut inner = Vec::new();
        for item in items {
            inner.extend_from_slice(&encode_value(base_type, types, item)?);
        }
        return Ok(keccak256(&inner).to_vec());
    }

    // Handle struct types (referenced custom types)
    if types.get(field_type).is_some() {
        let hash = compute_struct_hash(field_type, types, value)?;
        return Ok(hash.to_vec());
    }

    // Handle atomic types
    match field_type {
        "address" => {
            let addr_str = value.as_str().ok_or_else(|| {
                InputError::InvalidHexInput("expected address string".to_string())
            })?;
            let addr: Address = addr_str.parse().map_err(|_| ConfigError::InvalidAddress {
                context: "EIP-712 field",
                value: addr_str.to_string(),
            })?;
            let mut buf = [0u8; 32];
            buf[12..].copy_from_slice(addr.as_slice());
            Ok(buf.to_vec())
        }
        "bool" => {
            let b = value.as_bool().unwrap_or(false);
            let mut buf = [0u8; 32];
            if b {
                buf[31] = 1;
            }
            Ok(buf.to_vec())
        }
        "string" => {
            let s = value.as_str().unwrap_or("");
            Ok(keccak256(s.as_bytes()).to_vec())
        }
        "bytes" => {
            let hex_str = value.as_str().unwrap_or("0x");
            let hex_clean = hex_str.strip_prefix("0x").unwrap_or(hex_str);
            let bytes = hex::decode(hex_clean)
                .map_err(|_| InputError::InvalidHexInput("invalid bytes hex".to_string()))?;
            Ok(keccak256(&bytes).to_vec())
        }
        t if t.starts_with("bytes") => {
            // bytesN (fixed-size)
            let hex_str = value.as_str().unwrap_or("0x");
            let hex_clean = hex_str.strip_prefix("0x").unwrap_or(hex_str);
            let bytes = hex::decode(hex_clean)
                .map_err(|_| InputError::InvalidHexInput("invalid bytesN hex".to_string()))?;
            let mut buf = [0u8; 32];
            let len = bytes.len().min(32);
            buf[..len].copy_from_slice(&bytes[..len]);
            Ok(buf.to_vec())
        }
        t if t.starts_with("uint") || t.starts_with("int") => {
            let mut buf = [0u8; 32];
            if let Some(n) = value.as_u64() {
                buf[24..].copy_from_slice(&n.to_be_bytes());
            } else if let Some(s) = value.as_str() {
                if let Some(hex_val) = s.strip_prefix("0x") {
                    let bytes = hex::decode(hex_val)
                        .map_err(|_| InputError::InvalidHexInput("invalid uint hex".to_string()))?;
                    let start = 32 - bytes.len().min(32);
                    buf[start..start + bytes.len().min(32)]
                        .copy_from_slice(&bytes[..bytes.len().min(32)]);
                } else if let Ok(n) = s.parse::<u128>() {
                    buf[16..].copy_from_slice(&n.to_be_bytes());
                } else {
                    let n: u64 = s.parse().map_err(|_| {
                        InputError::InvalidHexInput(format!("invalid numeric value: {s}"))
                    })?;
                    buf[24..].copy_from_slice(&n.to_be_bytes());
                }
            } else if let Some(n) = value.as_i64() {
                buf[24..].copy_from_slice(&(n as u64).to_be_bytes());
            }
            Ok(buf.to_vec())
        }
        _ => {
            let hex_str = value.as_str().unwrap_or("0x");
            let hex_clean = hex_str.strip_prefix("0x").unwrap_or(hex_str);
            let bytes = hex::decode(hex_clean).unwrap_or_default();
            let mut buf = [0u8; 32];
            let len = bytes.len().min(32);
            buf[..len].copy_from_slice(&bytes[..len]);
            Ok(buf.to_vec())
        }
    }
}

fn build_api_base_url(auth_server_url: &str) -> Result<String, TempoError> {
    let url = url::Url::parse(auth_server_url).map_err(|source| InputError::UrlParseFor {
        context: "auth server",
        source,
    })?;
    Ok(url.origin().ascii_serialization())
}

fn build_transaction_data(
    to: &str,
    data: &str,
    value: &str,
) -> Result<serde_json::Value, TempoError> {
    if !is_zero_value(value)? {
        return Err(ConfigError::Invalid(
            "Coinflow credits redeem does not support non-zero ETH value".to_string(),
        )
        .into());
    }

    if data == "0x" {
        return Ok(serde_json::json!({
            "type": "token",
            "destination": to,
        }));
    }

    Ok(serde_json::json!({
        "transaction": {
            "to": to,
            "data": data,
        },
    }))
}

fn is_zero_value(value: &str) -> Result<bool, TempoError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(true);
    }

    if let Some(hex_value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        if !hex_value.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(InputError::InvalidHexInput(format!("invalid ETH value: {value}")).into());
        }
        return Ok(hex_value.is_empty() || hex_value.bytes().all(|byte| byte == b'0'));
    }

    if !value.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(InputError::InvalidHexInput(format!("invalid ETH value: {value}")).into());
    }

    Ok(value.bytes().all(|byte| byte == b'0'))
}

impl SpendCreditsResult {
    fn render(&self, format: OutputFormat) -> Result<(), TempoError> {
        output::emit_by_format(format, self, || {
            let w = &mut std::io::stdout();
            writeln!(w, "{:>10}: {}", "Wallet", self.wallet)?;
            writeln!(
                w,
                "{:>10}: ${:.2}",
                "Amount",
                self.amount_cents as f64 / 100.0
            )?;
            if self.dry_run {
                writeln!(w, "{:>10}: yes", "Dry Run")?;
            }
            if let Some(tx_hash) = &self.tx_hash {
                writeln!(w, "{:>10}: {}", "TX Hash", tx_hash)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy::primitives::Address;
    use tempo_common::{keys::Keystore, network::NetworkId};
    use tempo_primitives::transaction::TempoSignature;
    use zeroize::Zeroizing;

    const TEST_PRIVATE_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const TEST_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    const COINFLOW_AUTH_MSG: &str = r#"{"domain":{"name":"Coinflow Credits Contract","version":"1","chainId":42431,"verifyingContract":"0x02af2603e2A7d891684854CBC4aaeBa310bf7C1c"},"message":{"customerWallet":"0x480F8659821A7a5f6209cDA338A53E9Dea09DB46","creditSeed":"tempo-sandbox","amount":1030000,"validBefore":"1777483839","nonce":"0x7968399a1307417362f545e43d5a12eb942562dd8c181d41f68fd881f56ba23d"},"primaryType":"CreditsAuthorization","types":{"EIP712Domain":[{"name":"name","type":"string"},{"name":"version","type":"string"},{"name":"chainId","type":"uint256"},{"name":"verifyingContract","type":"address"}],"CreditsAuthorization":[{"name":"customerWallet","type":"address"},{"name":"creditSeed","type":"string"},{"name":"amount","type":"uint256"},{"name":"validBefore","type":"uint256"},{"name":"nonce","type":"bytes32"}]}}"#;

    #[test]
    fn compute_coinflow_auth_message_matches_reference_hash() {
        let digest = compute_eip712_signing_hash(COINFLOW_AUTH_MSG).unwrap();

        assert_eq!(
            format!("{digest:#x}"),
            "0x3caf17f85e96e489a081eab08cdc14794d8725d4356ef252fc98de3c65e03225"
        );
    }

    #[test]
    fn sign_coinflow_auth_message_with_access_key_matches_viem_keychain_envelope() {
        let mut keys = Keystore::default();
        let wallet: Address = "0x480f8659821a7a5f6209cda338a53e9dea09db46"
            .parse()
            .unwrap();
        let entry = keys.upsert_by_wallet_address_and_chain(wallet, 4217);
        entry.key_address = Some(TEST_ADDRESS.to_string());
        entry.key = Some(Zeroizing::new(TEST_PRIVATE_KEY.to_string()));
        let signer = keys.signer(NetworkId::Tempo).unwrap();
        let digest = compute_eip712_signing_hash(COINFLOW_AUTH_MSG).unwrap();
        let signature = signer
            .sign_hash_hex(&digest, "sign EIP-712 credits authorization")
            .unwrap();
        let signature_bytes = hex::decode(signature.trim_start_matches("0x")).unwrap();
        let parsed = TempoSignature::from_bytes(&signature_bytes).unwrap();
        let keychain = parsed.as_keychain().expect("expected keychain envelope");

        assert_eq!(
            signature,
            "0x04480f8659821a7a5f6209cda338a53e9dea09db46c940b8c39d08d4a737ed58543b2cd922debb9881fb6efc05ac4fa3269b0c28c13e04a2abe52b1aad61b0d5e1b79fe85fcef6f093878d237a7be2e12b1cb7c6c01b"
        );
        assert_eq!(signature.len(), 174, "0x + 86 byte keychain envelope");
        assert!(signature.starts_with("0x04"));
        assert_eq!(parsed.recover_signer(&digest).unwrap(), wallet);
        assert_eq!(keychain.key_id(&digest).unwrap(), signer.signer.address());
    }

    #[test]
    fn build_transaction_data_uses_token_redeem_shape_without_calldata() {
        let transaction_data = build_transaction_data(TEST_ADDRESS, "0x", "0").unwrap();

        assert_eq!(
            transaction_data,
            serde_json::json!({
                "type": "token",
                "destination": TEST_ADDRESS,
            })
        );
    }

    #[test]
    fn build_transaction_data_uses_normal_redeem_shape_with_calldata() {
        let transaction_data = build_transaction_data(TEST_ADDRESS, "0xdeadbeef", "0").unwrap();

        assert_eq!(
            transaction_data,
            serde_json::json!({
                "transaction": {
                    "to": TEST_ADDRESS,
                    "data": "0xdeadbeef",
                },
            })
        );
    }

    #[test]
    fn build_transaction_data_rejects_non_zero_eth_value() {
        let err = build_transaction_data(TEST_ADDRESS, "0xdeadbeef", "1").unwrap_err();

        assert!(err
            .to_string()
            .contains("Coinflow credits redeem does not support non-zero ETH value"));
    }

    #[test]
    fn parse_mpp_challenge_accepts_www_authenticate_header_line() {
        let request = mpp::Base64UrlJson::from_value(&serde_json::json!({
            "amount": "10000",
            "currency": "0x20C000000000000000000000b9537d11c60E8b50",
            "recipient": TEST_ADDRESS,
            "methodDetails": { "chainId": 4217 }
        }))
        .unwrap();
        let challenge = mpp::PaymentChallenge::new(
            "challenge-123",
            "api.example.com",
            "tempo",
            "charge",
            request,
        );
        let header = mpp::format_www_authenticate(&challenge).unwrap();

        let parsed = parse_mpp_challenge(&format!("WWW-Authenticate: {header}")).unwrap();

        assert_eq!(parsed.id, "challenge-123");
        assert_eq!(parsed.realm, "api.example.com");
    }

    #[test]
    fn converts_six_decimal_usdc_amount_to_coinflow_cents() {
        assert_eq!(amount_to_usd_cents(10_000, 6).unwrap(), 1);
        assert_eq!(amount_to_usd_cents(1_230_000, 6).unwrap(), 123);
    }

    #[test]
    fn rejects_mpp_amounts_that_do_not_fit_coinflow_cents() {
        let err = amount_to_usd_cents(1, 6).unwrap_err();

        assert!(err
            .to_string()
            .contains("cannot be represented exactly in Coinflow credits cents"));
    }

    #[test]
    fn parses_32_byte_mpp_memo_hex() {
        let memo =
            parse_memo_hex("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
                .unwrap();

        assert_eq!(memo[0], 0x12);
        assert_eq!(memo[31], 0xef);
    }

    #[test]
    fn detects_max_credits_authorized_mismatch() {
        assert!(is_max_credits_authorized_mismatch(
            r#"{"error":"Failed to send redeem transaction","detail":"HTTP 412: {\"message\":\"Error Processing your request\",\"details\":\"Total 1.04 exceeds max credits authorized 1.03\"}"}"#
        ));
    }

    #[test]
    fn ignores_other_coinflow_failures() {
        assert!(!is_max_credits_authorized_mismatch(
            r#"{"error":"Failed to send redeem transaction","detail":"HTTP 412: {\"message\":\"Error Processing your request\",\"details\":\"Wallet does not have enough credits to complete redeem request\"}"}"#
        ));
    }
}
