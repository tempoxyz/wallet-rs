//! Credits balance lookup.

use std::io::Write;

use serde::Serialize;

use crate::commands::fund;
use tempo_common::{
    cli::{context::Context, output, output::OutputFormat},
    error::TempoError,
};

#[derive(Debug, Serialize)]
struct CreditsResponse {
    wallet: String,
    balance: String,
    raw_balance: String,
}

pub(crate) async fn run(ctx: &Context) -> Result<(), TempoError> {
    let auth_server_url =
        std::env::var("TEMPO_AUTH_URL").unwrap_or_else(|_| ctx.network.auth_url().to_string());
    let wallet = fund::resolve_address(None, &ctx.keys)?;
    let raw_balance = fund::query_credit_balance(&auth_server_url, &wallet).await?;
    let response = CreditsResponse {
        wallet,
        balance: fund::format_credit_balance(raw_balance),
        raw_balance: raw_balance.to_string(),
    };

    response.render(ctx.output_format)
}

impl CreditsResponse {
    fn render(&self, format: OutputFormat) -> Result<(), TempoError> {
        output::emit_by_format(format, self, || {
            let w = &mut std::io::stdout();
            writeln!(w, "{:>10}: {}", "Wallet", self.wallet)?;
            writeln!(w, "{:>10}: {}", "Credits", self.balance)?;
            Ok(())
        })
    }
}
