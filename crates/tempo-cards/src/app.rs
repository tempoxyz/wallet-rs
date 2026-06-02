//! Application entry point: build context, dispatch command, flush analytics.

use crate::{
    args::{CardsCommands, Cli},
    commands::cards,
};
use tempo_common::error::TempoError;

/// Run the tempo-cards application.
pub(crate) async fn run(mut cli: Cli) -> Result<(), TempoError> {
    let command = if let Some(c) = cli.command.take() {
        c
    } else {
        use clap::CommandFactory;
        return Cli::command().print_help().map_err(Into::into);
    };

    tempo_common::cli::run_cli(
        &cli.global,
        &["tempo_cards"],
        "tempo-cards",
        |ctx| async move {
            let cmd_name = command_name(&command);
            let result = cards::run(&ctx, Some(command)).await;
            (cmd_name, result)
        },
    )
    .await
}

/// Derive a short analytics-friendly name from a parsed command.
const fn command_name(command: &CardsCommands) -> &'static str {
    match command {
        CardsCommands::Config { .. } => "cards config",
        CardsCommands::Customers { .. } => "cards customers",
        CardsCommands::Create { .. } => "cards create",
        CardsCommands::List { .. } => "cards list",
        CardsCommands::Get { .. } => "cards get",
        CardsCommands::Update { .. } => "cards update",
        CardsCommands::Freeze { .. } => "cards freeze",
        CardsCommands::Unfreeze { .. } => "cards unfreeze",
        CardsCommands::Cancel { .. } => "cards cancel",
        CardsCommands::Cardholders { .. } => "cards cardholders",
        CardsCommands::Transactions { .. } => "cards transactions",
        CardsCommands::Authorizations { .. } => "cards authorizations",
        CardsCommands::Approve { .. } => "cards approve",
        CardsCommands::Allowance { .. } => "cards allowance",
    }
}
