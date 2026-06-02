//! Wallet approval and allowance helpers for card spend.

use alloy::{
    primitives::{
        address,
        utils::{format_units, parse_units, ParseUnits},
        Address, Bytes, TxKind, U256,
    },
    providers::ProviderBuilder,
    sol,
    sol_types::SolCall,
};
use serde::Serialize;
use tempo_primitives::transaction::Call;

use tempo_common::{
    cli::{context::Context, output},
    error::{ConfigError, InputError, NetworkError, TempoError},
    network::NetworkId,
    payment::session::submit_tempo_tx,
};

sol! {
    #[sol(rpc)]
    interface ITIP20Cards {
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

/// Tempo mainnet card issuer spender from the wallet-backed cards flow.
const TEMPO_CARDS_ISSUER: Address = address!("3e8f24b686aa8c036038f7d557b70e6ce0e7b56b");

#[derive(Debug, Serialize)]
struct ApprovalResponse {
    status: &'static str,
    wallet: String,
    spender: String,
    token: String,
    symbol: &'static str,
    amount: String,
    amount_atomic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<String>,
}

#[derive(Debug, Serialize)]
struct AllowanceResponse {
    wallet: String,
    spender: String,
    token: String,
    symbol: &'static str,
    allowance: String,
    allowance_atomic: String,
}

pub(super) async fn approve(
    ctx: &Context,
    amount: String,
    spender_input: Option<String>,
    fee_token_input: Option<String>,
    dry_run: bool,
) -> Result<(), TempoError> {
    ctx.keys.ensure_key_for_network(ctx.network)?;

    let token = ctx.network.token();
    let spender = resolve_spender(ctx.network, spender_input.as_deref())?;
    let wallet = ctx.keys.signer(ctx.network)?;
    let from = wallet.from;
    let (amount_atomic, amount_human) = parse_allowance_amount(&amount, token.decimals)?;
    let fee_token = fee_token_input.map_or_else(
        || Ok(token.address),
        |input| {
            tempo_common::security::parse_address_input(&input, "fee token")
                .map_err(TempoError::from)
        },
    )?;

    if dry_run {
        let response = ApprovalResponse {
            status: "dry_run",
            wallet: format!("{from:#x}"),
            spender: format!("{spender:#x}"),
            token: format!("{:#x}", token.address),
            symbol: token.symbol,
            amount: amount_human,
            amount_atomic: amount_atomic.to_string(),
            tx_hash: None,
        };
        return output::emit_by_format(ctx.output_format, &response, || {
            println!("{}", serde_json::to_string_pretty(&response)?);
            Ok(())
        });
    }

    let approve_data = Bytes::from(
        ITIP20Cards::approveCall {
            spender,
            amount: amount_atomic,
        }
        .abi_encode(),
    );
    let calls = vec![Call {
        to: TxKind::Call(token.address),
        value: U256::ZERO,
        input: approve_data,
    }];

    let rpc_url = ctx.config.rpc_url(ctx.network);
    let tempo_provider =
        alloy::providers::RootProvider::<mpp::client::TempoNetwork>::new_http(rpc_url);
    let tx_hash = submit_tempo_tx(
        &tempo_provider,
        &wallet,
        ctx.network.chain_id(),
        fee_token,
        from,
        calls,
    )
    .await?;

    let response = ApprovalResponse {
        status: "success",
        wallet: format!("{from:#x}"),
        spender: format!("{spender:#x}"),
        token: format!("{:#x}", token.address),
        symbol: token.symbol,
        amount: amount_human,
        amount_atomic: amount_atomic.to_string(),
        tx_hash: Some(tx_hash.clone()),
    };
    output::emit_by_format(ctx.output_format, &response, || {
        eprintln!("Approved card issuer spend.");
        eprintln!("  TX: {tx_hash}");
        eprintln!("  {}", ctx.network.tx_url(&tx_hash));
        Ok(())
    })
}

pub(super) async fn allowance(
    ctx: &Context,
    spender_input: Option<String>,
    wallet_address_input: Option<String>,
) -> Result<(), TempoError> {
    let token = ctx.network.token();
    let spender = resolve_spender(ctx.network, spender_input.as_deref())?;
    let owner = if let Some(input) = wallet_address_input {
        tempo_common::security::parse_address_input(&input, "wallet address")?
    } else {
        ctx.keys
            .key_for_network(ctx.network)
            .and_then(|entry| entry.wallet_address_parsed())
            .ok_or_else(|| {
                ConfigError::Missing(
                    "No wallet configured. Run `tempo wallet login` or pass --wallet-address."
                        .to_string(),
                )
            })?
    };

    let rpc_url = ctx.config.rpc_url(ctx.network);
    let provider = ProviderBuilder::new().connect_http(rpc_url);
    let contract = ITIP20Cards::new(token.address, provider);
    let allowance = contract
        .allowance(owner, spender)
        .call()
        .await
        .map_err(|source| NetworkError::RpcSource {
            operation: "query card issuer allowance",
            source: Box::new(source),
        })?;
    let allowance_human = format_units(allowance, token.decimals).expect("decimals <= 77");

    let response = AllowanceResponse {
        wallet: format!("{owner:#x}"),
        spender: format!("{spender:#x}"),
        token: format!("{:#x}", token.address),
        symbol: token.symbol,
        allowance: allowance_human,
        allowance_atomic: allowance.to_string(),
    };
    output::emit_by_format(ctx.output_format, &response, || {
        println!("{}", serde_json::to_string_pretty(&response)?);
        Ok(())
    })
}

fn resolve_spender(network: NetworkId, input: Option<&str>) -> Result<Address, TempoError> {
    if let Some(input) = input {
        return tempo_common::security::parse_address_input(input, "spender").map_err(Into::into);
    }
    match network {
        NetworkId::Tempo => Ok(TEMPO_CARDS_ISSUER),
        NetworkId::TempoModerato => Err(ConfigError::Missing(
            "No default card issuer spender is configured for tempo-moderato. Pass --spender."
                .to_string(),
        )
        .into()),
    }
}

fn parse_allowance_amount(input: &str, decimals: u8) -> Result<(U256, String), TempoError> {
    if matches!(input, "max" | "MAX" | "unlimited" | "UNLIMITED") {
        return Ok((U256::MAX, "max".to_string()));
    }

    let parsed = parse_units(input, decimals)
        .map_err(|_| InputError::InvalidHexInput(format!("Invalid amount: '{input}'")))?;
    let amount = match parsed {
        ParseUnits::U256(value) => value,
        ParseUnits::I256(value) => {
            if value.is_negative() {
                return Err(
                    InputError::InvalidHexInput("Amount must be positive.".to_string()).into(),
                );
            }
            value.into_raw()
        }
    };
    if amount.is_zero() {
        return Err(
            InputError::InvalidHexInput("Amount must be greater than zero.".to_string()).into(),
        );
    }
    Ok((amount, input.to_string()))
}
