#![allow(
    clippy::cast_possible_truncation,
    clippy::items_after_statements,
    clippy::manual_let_else,
    clippy::needless_pass_by_value,
    clippy::option_if_let_else,
    clippy::redundant_pub_crate,
    clippy::struct_field_names,
    clippy::too_many_lines,
    clippy::unnecessary_wraps,
    clippy::unused_async
)]
//! CLI entry point for `tempo-cards`.

mod app;
mod args;
mod commands;

use crate::args::Cli;

#[tokio::main]
async fn main() {
    let cli: Cli = tempo_common::cli::parse_cli();
    let output_format = cli.global.resolve_output_format();
    let result = app::run(cli).await;
    tempo_common::cli::run_main(output_format, result);
}
