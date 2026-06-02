//! Application entry point: build context, dispatch command, flush analytics.

use crate::{
    args::{Cli, Commands, ServicesCommands, SessionCommands},
    commands::{
        completions, credits, debug, fund, keys, login, logout, refresh, services, sessions,
        spend_credits, transfer, whoami,
    },
};
use tempo_common::error::{ConfigError, TempoError};

/// Run the tempo-wallet application.
pub(crate) async fn run(mut cli: Cli) -> Result<(), TempoError> {
    let command = if let Some(c) = cli.command.take() {
        c
    } else {
        use clap::CommandFactory;
        return Cli::command().print_help().map_err(Into::into);
    };

    tempo_common::cli::run_cli(
        &cli.global,
        &["tempo_wallet"],
        "tempo-wallet",
        |ctx| async move {
            let cmd_name = command_name(&command);
            let result = match command {
                Commands::Login { no_browser } => login::run(&ctx, no_browser).await,
                Commands::Refresh => refresh::run(&ctx).await,
                Commands::Logout { yes } => logout::run(&ctx, yes),
                Commands::Completions { shell } => completions::run(&ctx, shell),
                Commands::Fund {
                    address,
                    no_browser,
                    crypto,
                    credits,
                    referral_code,
                } => {
                    let target = fund::Target::from_cli(crypto, credits, referral_code);
                    fund::run(&ctx, address, no_browser, target).await
                }
                Commands::Whoami { credits: true } => credits::run(&ctx).await,
                Commands::Whoami { .. } => whoami::run(&ctx).await,
                Commands::Keys => keys::run(&ctx).await,
                Commands::Sessions { command } => {
                    sessions::run(
                        &ctx,
                        command.unwrap_or(SessionCommands::List {
                            orphaned: false,
                            all: false,
                        }),
                    )
                    .await
                }
                Commands::Transfer {
                    credits: true,
                    amount_cents,
                    credits_to,
                    data,
                    value,
                    mpp_challenge,
                    mpp_challenge_file,
                    mpp_client_id,
                    address,
                    ..
                } => {
                    if let Some(challenge) = mpp_challenge {
                        if amount_cents.is_some() || credits_to.is_some() || data != "0x" || value != "0" {
                            Err(ConfigError::Invalid(
                                "--mpp-challenge cannot be combined with --amount-cents, --to, --data, or --value".to_string(),
                            )
                            .into())
                        } else {
                            spend_credits::run_mpp(&ctx, challenge, mpp_client_id, address).await
                        }
                    } else if let Some(challenge_path) = mpp_challenge_file {
                        if amount_cents.is_some() || credits_to.is_some() || data != "0x" || value != "0" {
                            Err(ConfigError::Invalid(
                                "--mpp-challenge-file cannot be combined with --amount-cents, --to, --data, or --value".to_string(),
                            )
                            .into())
                        } else {
                            spend_credits::run_mpp_file(
                                &ctx,
                                &challenge_path,
                                mpp_client_id,
                                address,
                            )
                            .await
                        }
                    } else {
                        match (amount_cents, credits_to) {
                            (Some(amount_cents), Some(to)) => {
                                spend_credits::run(&ctx, amount_cents, to, data, value, address)
                                    .await
                            }
                            _ => Err(ConfigError::Missing(
                                "--amount-cents and --to are required when using --credits, unless --mpp-challenge is provided".to_string(),
                            )
                            .into()),
                        }
                    }
                }
                Commands::Transfer {
                    amount,
                    token,
                    to,
                    fee_token,
                    dry_run,
                    ..
                } => {
                    let amount = amount.expect("required for token transfers");
                    let token = token.expect("required for token transfers");
                    let to = to.expect("required for token transfers");
                    transfer::run(&ctx, amount, token, to, fee_token, dry_run).await
                }
                Commands::Debug => debug::run(&ctx).await,
                Commands::Services {
                    service_id, search, ..
                } => services::run(&ctx, services::ServicesArgs { service_id, search }).await,
            };
            (cmd_name, result)
        },
    )
    .await
}

/// Derive a short analytics-friendly name from a parsed command.
const fn command_name(command: &Commands) -> &'static str {
    match command {
        Commands::Login { .. } => "login",
        Commands::Refresh => "refresh",
        Commands::Logout { .. } => "logout",
        Commands::Completions { .. } => "completions",
        Commands::Fund { .. } => "fund",
        Commands::Whoami { .. } => "whoami",
        Commands::Keys => "keys",
        Commands::Sessions { command } => match command {
            Some(SessionCommands::List { .. }) | None => "sessions list",
            Some(SessionCommands::Close { .. }) => "sessions close",
            Some(SessionCommands::Sync { .. }) => "sessions sync",
        },
        Commands::Transfer { .. } => "transfer",
        Commands::Debug => "debug",
        Commands::Services { command, .. } => match command {
            Some(ServicesCommands::List) => "services list",
            None => "services",
        },
    }
}
