//! CLI argument definitions and parsing.

use clap::{Parser, Subcommand};

/// Long version string including git commit, build date, and profile.
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("TEMPO_GIT_SHA"),
    " ",
    env!("TEMPO_BUILD_DATE"),
    " ",
    env!("TEMPO_BUILD_PROFILE"),
    ")"
);

#[derive(Parser, Debug)]
#[command(name = "tempo wallet")]
#[command(about = "Wallet identity and custody operations", long_about = None)]
#[command(version = LONG_VERSION)]
#[command(override_usage = "\n  tempo wallet <COMMAND> [OPTIONS]")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    #[command(flatten)]
    pub global: tempo_common::cli::GlobalArgs,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// Sign up or log in to your Tempo wallet
    #[command(display_order = 1)]
    Login {
        /// Do not attempt to open a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Refresh your access key without logging out
    #[command(display_order = 2)]
    Refresh,
    /// Log out and disconnect your wallet
    #[command(display_order = 3)]
    Logout {
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Show who you are: wallet, balances, keys
    #[command(display_order = 4)]
    Whoami,
    /// List keys and their spending limits
    #[command(display_order = 5, name = "keys")]
    Keys,
    /// Transfer tokens to an address
    #[command(display_order = 6, arg_required_else_help = true)]
    #[command(after_help = "\
Examples:
  tempo wallet transfer 1.00 0x20c0...b50 0x70997...9C8
  tempo wallet transfer 50 0x20c0...b50 0x70997...9C8 --dry-run")]
    Transfer {
        /// Amount in human units ("1.00", "50")
        amount: String,
        /// Token contract address (0x...)
        token: String,
        /// Recipient address (0x...)
        to: String,
        /// Pay fees in a different token (default: same token)
        #[arg(long)]
        fee_token: Option<String>,
        /// Show plan + fee estimate, don't send
        #[arg(long)]
        dry_run: bool,
    },
    /// Fund your wallet (testnet faucet or mainnet bridge)
    #[command(display_order = 7, name = "fund")]
    Fund {
        /// Wallet address to fund (defaults to current wallet)
        #[arg(long)]
        address: Option<String>,
        /// Do not attempt to open a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Manage payment sessions
    #[command(display_order = 8, name = "sessions")]
    #[command(args_conflicts_with_subcommands = true)]
    Sessions {
        #[command(subcommand)]
        command: Option<SessionCommands>,
    },
    /// Browse the MPP service directory
    #[command(display_order = 9, name = "services")]
    Services {
        #[command(subcommand)]
        command: Option<ServicesCommands>,

        /// Service ID to show details for
        #[arg(value_name = "SERVICE_ID")]
        service_id: Option<String>,

        /// Search by name, description, tags, or category
        #[arg(long, value_name = "QUERY")]
        search: Option<String>,
    },
    /// Collect debug info for support
    #[command(display_order = 10)]
    Debug,

    /// Generate shell completions script
    #[command(hide = true)]
    Completions {
        /// The shell to generate completions for
        #[arg(value_enum)]
        shell: Option<clap_complete::Shell>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SessionCommands {
    /// List payment sessions
    List {
        /// Include on-chain orphaned discovery and persist discovered channels locally
        #[arg(long)]
        orphaned: bool,
        /// Include local sessions and on-chain orphaned discovery in one view
        #[arg(long)]
        all: bool,
    },
    /// Close a payment session and remove it locally
    Close {
        /// URL, origin, or channel ID (0x...) to close
        url: Option<String>,
        /// Close all active sessions and on-chain channels
        #[arg(long)]
        all: bool,
        /// Close only orphaned on-chain channels (no local session)
        #[arg(long)]
        orphaned: bool,
        /// Finalize channels pending close (grace period elapsed)
        #[arg(long)]
        finalize: bool,
        /// Use cooperative close only (no on-chain fallback)
        #[arg(long)]
        cooperative: bool,
        /// Show what would be closed without executing
        #[arg(long)]
        dry_run: bool,
    },
    /// Sync local sessions with on-chain state
    ///
    /// Without flags, removes stale local records for settled channels.
    /// With `--origin`, re-syncs close timing for a specific session from
    /// on-chain state. Useful after crashes or manual DB edits.
    Sync {
        /// Re-sync a specific origin's close state from on-chain
        #[arg(long)]
        origin: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ServicesCommands {
    /// List available services
    List,
}
