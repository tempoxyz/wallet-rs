//! Tempo wallet-backed card commands.

mod approval;
mod client;
mod config;

use serde::Serialize;
use serde_json::json;

use tempo_common::{
    cli::{context::Context, output},
    error::{ConfigError, TempoError},
    network::NetworkId,
};

use crate::args::{
    CardCancellationReason, CardsAuthorizationCommands, CardsCardholderCommands, CardsCommands,
    CardsConfigCommands, CardsCustomerCommands, CardsTransactionCommands, IssuingCardStatus,
};

use self::{
    client::{maybe_push, path_segment, BridgeClient, StripeClient},
    config::{
        bridge_api_key, bridge_environment, cards_config_path, mask_secret, stripe_api_key,
        stripe_mode, CardsConfig,
    },
};

pub(crate) async fn run(ctx: &Context, command: Option<CardsCommands>) -> Result<(), TempoError> {
    let Some(command) = command else {
        return Err(ConfigError::Missing("cards command required".to_string()).into());
    };

    match command {
        CardsCommands::Config { command } => run_config(ctx, command),
        CardsCommands::Customers { command } => run_customers(ctx, command).await,
        CardsCommands::Create {
            cardholder,
            wallet_address,
            idempotency_key,
            bridge_customer_id,
        } => {
            run_card_create(
                ctx,
                cardholder,
                wallet_address,
                idempotency_key,
                bridge_customer_id,
            )
            .await
        }
        CardsCommands::List {
            cardholder,
            status,
            card_type,
            last4,
            limit,
            starting_after,
            ending_before,
        } => {
            let mut params = Vec::new();
            maybe_push(&mut params, "cardholder", cardholder);
            maybe_push(
                &mut params,
                "status",
                status.map(|s| s.as_str().to_string()),
            );
            maybe_push(
                &mut params,
                "type",
                card_type.map(|t| t.as_str().to_string()),
            );
            maybe_push(&mut params, "last4", last4);
            maybe_push(&mut params, "limit", limit.map(|v| v.to_string()));
            maybe_push(&mut params, "starting_after", starting_after);
            maybe_push(&mut params, "ending_before", ending_before);
            let value = StripeClient::new()?
                .get("/issuing/cards", &params, "list issuing cards")
                .await?;
            emit_api_value(ctx, &value)
        }
        CardsCommands::Get { id } => {
            let value = StripeClient::new()?
                .get(
                    &format!("/issuing/cards/{}", path_segment(&id)),
                    &[],
                    "get issuing card",
                )
                .await?;
            emit_api_value(ctx, &value)
        }
        CardsCommands::Update {
            id,
            status,
            cancellation_reason,
        } => {
            let mut form = vec![("status", status.as_str().to_string())];
            if let Some(reason) = cancellation_reason {
                form.push(("cancellation_reason", reason.as_str().to_string()));
            }
            let value = StripeClient::new()?
                .post_form(
                    &format!("/issuing/cards/{}", path_segment(&id)),
                    form,
                    None,
                    "update issuing card",
                )
                .await?;
            emit_api_value(ctx, &value)
        }
        CardsCommands::Freeze { id } => {
            update_card_status(ctx, id, IssuingCardStatus::Inactive, None).await
        }
        CardsCommands::Unfreeze { id } => {
            update_card_status(ctx, id, IssuingCardStatus::Active, None).await
        }
        CardsCommands::Cancel {
            id,
            cancellation_reason,
        } => update_card_status(ctx, id, IssuingCardStatus::Canceled, cancellation_reason).await,
        CardsCommands::Cardholders { command } => run_cardholders(ctx, command).await,
        CardsCommands::Transactions { command } => run_transactions(ctx, command).await,
        CardsCommands::Authorizations { command } => run_authorizations(ctx, command).await,
        CardsCommands::Approve {
            amount,
            spender,
            fee_token,
            dry_run,
        } => approval::approve(ctx, amount, spender, fee_token, dry_run).await,
        CardsCommands::Allowance {
            spender,
            wallet_address,
        } => approval::allowance(ctx, spender, wallet_address).await,
    }
}

fn run_config(ctx: &Context, command: CardsConfigCommands) -> Result<(), TempoError> {
    match command {
        CardsConfigCommands::BridgeApiKey { api_key } => {
            let mut config = CardsConfig::load()?;
            config.bridge_api_key = Some(api_key.clone());
            let path = config.save()?;
            let response = json!({
                "saved": true,
                "key": "bridge",
                "environment": bridge_environment(&api_key),
                "config": path.display().to_string(),
            });
            emit_api_value(ctx, &response)
        }
        CardsConfigCommands::StripeApiKey { api_key } => {
            let mut config = CardsConfig::load()?;
            config.stripe_api_key = Some(api_key.clone());
            let path = config.save()?;
            let response = json!({
                "saved": true,
                "key": "stripe",
                "mode": stripe_mode(&api_key),
                "config": path.display().to_string(),
            });
            emit_api_value(ctx, &response)
        }
        CardsConfigCommands::Show => {
            let bridge = bridge_api_key()?;
            let stripe = stripe_api_key()?;
            let response = json!({
                "bridge": bridge.as_ref().map_or_else(
                    || json!({ "api_key": null, "environment": null, "source": null }),
                    |secret| json!({
                        "api_key": mask_secret(&secret.value),
                        "environment": bridge_environment(&secret.value),
                        "source": secret.source.as_str(),
                    }),
                ),
                "stripe": stripe.as_ref().map_or_else(
                    || json!({ "api_key": null, "mode": null, "source": null }),
                    |secret| json!({
                        "api_key": mask_secret(&secret.value),
                        "mode": stripe_mode(&secret.value),
                        "source": secret.source.as_str(),
                    }),
                ),
                "config": cards_config_path()?.display().to_string(),
            });
            emit_api_value(ctx, &response)
        }
    }
}

async fn run_customers(ctx: &Context, command: CardsCustomerCommands) -> Result<(), TempoError> {
    let client = BridgeClient::new()?;
    let value = match command {
        CardsCustomerCommands::Create {
            customer_type,
            first_name,
            last_name,
            email,
        } => {
            client
                .post(
                    "/customers",
                    Some(json!({
                        "type": customer_type.as_str(),
                        "first_name": first_name,
                        "last_name": last_name,
                        "email": email,
                    })),
                    "create Bridge customer",
                )
                .await?
        }
        CardsCustomerCommands::Get { id } => {
            client
                .get(
                    &format!("/customers/{}", path_segment(&id)),
                    &[],
                    "get Bridge customer",
                )
                .await?
        }
        CardsCustomerCommands::List => {
            client
                .get("/customers", &[], "list Bridge customers")
                .await?
        }
        CardsCustomerCommands::Delete { id } => {
            client
                .delete(
                    &format!("/customers/{}", path_segment(&id)),
                    "delete Bridge customer",
                )
                .await?
        }
        CardsCustomerCommands::TosLink => {
            client
                .post("/customers/tos_links", Some(json!({})), "create ToS link")
                .await?
        }
        CardsCustomerCommands::TosAcceptanceLink { id } => {
            client
                .get(
                    &format!("/customers/{}/tos_acceptance_link", path_segment(&id)),
                    &[],
                    "get ToS acceptance link",
                )
                .await?
        }
        CardsCustomerCommands::KycLink {
            id,
            endorsement,
            redirect_uri,
        } => {
            let mut params = Vec::new();
            maybe_push(&mut params, "endorsement", endorsement);
            maybe_push(&mut params, "redirect_uri", redirect_uri);
            client
                .get(
                    &format!("/customers/{}/kyc_link", path_segment(&id)),
                    &params,
                    "get KYC link",
                )
                .await?
        }
        CardsCustomerCommands::Transfers { id } => {
            client
                .get(
                    &format!("/customers/{}/transfers", path_segment(&id)),
                    &[],
                    "list Bridge customer transfers",
                )
                .await?
        }
    };
    emit_api_value(ctx, &value)
}

async fn run_card_create(
    ctx: &Context,
    cardholder: String,
    wallet_address_input: Option<String>,
    idempotency_key: Option<String>,
    bridge_customer_id: Option<String>,
) -> Result<(), TempoError> {
    let wallet_address = resolve_card_wallet_address(ctx, wallet_address_input)?;
    let idempotency_key =
        idempotency_key.unwrap_or_else(|| format!("tempo-cards-{wallet_address}-{cardholder}"));

    let mut form = vec![
        ("cardholder", cardholder.clone()),
        ("currency", "usd".to_string()),
        ("type", "virtual".to_string()),
        ("status", "active".to_string()),
        ("crypto_wallet[chain]", "tempo".to_string()),
        ("crypto_wallet[currency]", "usdc".to_string()),
        ("crypto_wallet[type]", "standard".to_string()),
        ("crypto_wallet[address]", wallet_address.clone()),
        ("metadata[tempo_wallet]", wallet_address.clone()),
    ];
    if let Some(bridge_customer_id) = bridge_customer_id {
        form.push(("metadata[bridge_customer_id]", bridge_customer_id));
    }

    let value = StripeClient::new()?
        .post_form(
            "/issuing/cards",
            form,
            Some(&idempotency_key),
            "create issuing card",
        )
        .await?;
    emit_api_value(ctx, &value)
}

async fn update_card_status(
    ctx: &Context,
    id: String,
    status: IssuingCardStatus,
    cancellation_reason: Option<CardCancellationReason>,
) -> Result<(), TempoError> {
    let mut form = vec![("status", status.as_str().to_string())];
    if let Some(reason) = cancellation_reason {
        form.push(("cancellation_reason", reason.as_str().to_string()));
    }
    let value = StripeClient::new()?
        .post_form(
            &format!("/issuing/cards/{}", path_segment(&id)),
            form,
            None,
            "update issuing card",
        )
        .await?;
    emit_api_value(ctx, &value)
}

async fn run_cardholders(
    ctx: &Context,
    command: CardsCardholderCommands,
) -> Result<(), TempoError> {
    let client = StripeClient::new()?;
    let value = match command {
        CardsCardholderCommands::List {
            email,
            status,
            cardholder_type,
            limit,
            starting_after,
            ending_before,
        } => {
            let mut params = Vec::new();
            maybe_push(&mut params, "email", email);
            maybe_push(
                &mut params,
                "status",
                status.map(|s| s.as_str().to_string()),
            );
            maybe_push(
                &mut params,
                "type",
                cardholder_type.map(|t| t.as_str().to_string()),
            );
            maybe_push(&mut params, "limit", limit.map(|v| v.to_string()));
            maybe_push(&mut params, "starting_after", starting_after);
            maybe_push(&mut params, "ending_before", ending_before);
            client
                .get("/issuing/cardholders", &params, "list issuing cardholders")
                .await?
        }
        CardsCardholderCommands::Get { id } => {
            client
                .get(
                    &format!("/issuing/cardholders/{}", path_segment(&id)),
                    &[],
                    "get issuing cardholder",
                )
                .await?
        }
    };
    emit_api_value(ctx, &value)
}

async fn run_transactions(
    ctx: &Context,
    command: CardsTransactionCommands,
) -> Result<(), TempoError> {
    let client = StripeClient::new()?;
    let value = match command {
        CardsTransactionCommands::List {
            card,
            cardholder,
            transaction_type,
            limit,
            starting_after,
            ending_before,
        } => {
            let mut params = Vec::new();
            maybe_push(&mut params, "card", card);
            maybe_push(&mut params, "cardholder", cardholder);
            maybe_push(
                &mut params,
                "type",
                transaction_type.map(|t| t.as_str().to_string()),
            );
            maybe_push(&mut params, "limit", limit.map(|v| v.to_string()));
            maybe_push(&mut params, "starting_after", starting_after);
            maybe_push(&mut params, "ending_before", ending_before);
            client
                .get(
                    "/issuing/transactions",
                    &params,
                    "list issuing transactions",
                )
                .await?
        }
        CardsTransactionCommands::Get { id } => {
            client
                .get(
                    &format!("/issuing/transactions/{}", path_segment(&id)),
                    &[],
                    "get issuing transaction",
                )
                .await?
        }
    };
    emit_api_value(ctx, &value)
}

async fn run_authorizations(
    ctx: &Context,
    command: CardsAuthorizationCommands,
) -> Result<(), TempoError> {
    let client = StripeClient::new()?;
    let value = match command {
        CardsAuthorizationCommands::List {
            card,
            cardholder,
            status,
            limit,
            starting_after,
            ending_before,
        } => {
            let mut params = Vec::new();
            maybe_push(&mut params, "card", card);
            maybe_push(&mut params, "cardholder", cardholder);
            maybe_push(
                &mut params,
                "status",
                status.map(|s| s.as_str().to_string()),
            );
            maybe_push(&mut params, "limit", limit.map(|v| v.to_string()));
            maybe_push(&mut params, "starting_after", starting_after);
            maybe_push(&mut params, "ending_before", ending_before);
            client
                .get(
                    "/issuing/authorizations",
                    &params,
                    "list issuing authorizations",
                )
                .await?
        }
        CardsAuthorizationCommands::Get { id } => {
            client
                .get(
                    &format!("/issuing/authorizations/{}", path_segment(&id)),
                    &[],
                    "get issuing authorization",
                )
                .await?
        }
    };
    emit_api_value(ctx, &value)
}

fn resolve_card_wallet_address(
    ctx: &Context,
    wallet_address_input: Option<String>,
) -> Result<String, TempoError> {
    if let Some(input) = wallet_address_input {
        let address = tempo_common::security::parse_address_input(&input, "wallet address")?;
        return Ok(format!("{address:#x}"));
    }

    if ctx.network != NetworkId::Tempo {
        return Err(ConfigError::Invalid(
            "cards create defaults to a Tempo mainnet wallet. Pass --network tempo or provide --wallet-address explicitly.".to_string(),
        )
        .into());
    }

    ctx.keys.ensure_key_for_network(ctx.network)?;
    let address = ctx
        .keys
        .key_for_network(ctx.network)
        .and_then(|entry| entry.wallet_address_parsed())
        .ok_or_else(|| ConfigError::Missing("No wallet address configured.".to_string()))?;
    Ok(format!("{address:#x}"))
}

fn emit_api_value(ctx: &Context, value: &impl Serialize) -> Result<(), TempoError> {
    output::emit_by_format(ctx.output_format, value, || {
        println!("{}", serde_json::to_string_pretty(value)?);
        Ok(())
    })
}
