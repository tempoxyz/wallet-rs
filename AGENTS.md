# AGENTS.md

## Repository Overview

This is a Cargo workspace containing 5 crates under `crates/`, providing a command-line HTTP client with built-in [MPP](https://mpp.dev) payment support, wallet identity management, wallet-backed cards, and a release signing tool. The top-level `tempo` launcher lives in the main tempo repo (`tempo/crates/ext/`).

**Supported Payment Protocols:**
- [Machine Payments Protocol (MPP)](https://mpp.dev) - Open protocol for HTTP-native machine-to-machine payments

### Workspace Structure

The root `Cargo.toml` is workspace-only (no package). All dependencies are declared as `[workspace.dependencies]` in the root and consumed via `dep.workspace = true` in each crate. All crates live under `crates/`:

#### `crates/tempo-common/` — package `tempo-common` (library)

Shared library used by `tempo-wallet`, `tempo-request`, and `tempo-cards`. Contains core logic:
- `crates/tempo-common/src/lib.rs` - Module declarations (analytics, cli, config, error, keys, network, payment, security)
- `crates/tempo-common/src/analytics.rs` - Opt-out telemetry (PostHog)
- `crates/tempo-common/src/config.rs` - Configuration file handling
- `crates/tempo-common/src/error.rs` - Error types (ConfigError, TempoError, etc.)
- `crates/tempo-common/src/network.rs` - Network definitions (`NetworkId`), explorer config, RPC
- `crates/tempo-common/src/security.rs` - Security utilities (safe logging, sanitization, redaction)
- `crates/tempo-common/src/cli/` - Shared CLI infrastructure
  - `mod.rs` - Re-exports (parse_cli, GlobalArgs, run_cli, run_main, Verbosity)
  - `args.rs` - GlobalArgs, parse_cli
  - `context.rs` - `Context` struct (Config, NetworkId, Keystore, Analytics, OutputFormat, Verbosity)
  - `exit_codes.rs` - Process exit codes (ExitCode enum)
  - `format.rs` - Value formatting helpers (amounts, durations, timestamps)
  - `output.rs` - OutputFormat, structured output helpers
  - `runner.rs` - CLI lifecycle (run_cli, run_main)
  - `runtime.rs` - Tracing, color mode, error rendering
  - `terminal.rs` - Terminal output helpers (hyperlinks, field formatting, truncation, sanitization)
  - `tracking.rs` - Analytics tracking (track_command, track_result)
  - `verbosity.rs` - Verbosity configuration
- `crates/tempo-common/src/keys/` - Key storage (model, I/O), signer resolution, authorization
  - `mod.rs`, `model.rs`, `keystore.rs`, `io.rs`, `signer.rs`, `authorization.rs`
- `crates/tempo-common/src/payment/` - Payment error classification and session management
  - `mod.rs` - (classify, session)
  - `classify.rs` - Payment error classification and extraction
  - `session/` - Channel persistence and channel management (channel.rs, close.rs, store.rs, tx.rs)

#### `crates/tempo-wallet/` — package `tempo-wallet`, binary `tempo-wallet`

Wallet identity and custody extension, plus session/service management. Source organized by module directories:
- `crates/tempo-wallet/src/main.rs` - CLI entry point
- `crates/tempo-wallet/src/args.rs` - clap definitions (Cli, Commands, SessionCommands, ServicesCommands)
- `crates/tempo-wallet/src/app.rs` - Command dispatch: context building, command routing, analytics
- `crates/tempo-wallet/src/analytics.rs` - Wallet-specific analytics events and payloads
- `crates/tempo-wallet/src/prompt.rs` - Interactive prompt helpers
- `crates/tempo-wallet/src/wallet/` - Wallet account types (balances, keys, spending limits) and on-chain queries
  - `mod.rs`, `types.rs`, `query.rs`, `render.rs`
- `crates/tempo-wallet/src/commands/` - Command implementations (all take `&Context` as first arg)
  - `login.rs` - Login command (passkey authentication flow)
  - `logout.rs` - Logout command
  - `whoami.rs` - Whoami command
  - `keys.rs` - Key listing, balance and spending limit queries
  - `fund/` - Fund command (browser-based flow)
  - `sessions/` - Session management (list, close, sync, render)
  - `services/` - Service directory (client, model, render)
  - `sign.rs` - Sign MPP payment challenges
  - `completions.rs` - Shell completions
- `crates/tempo-wallet/tests/` - Integration tests (black-box CLI testing via assert_cmd)

#### `crates/tempo-cards/` — package `tempo-cards`, binary `tempo-cards`

Wallet-backed card extension invoked as `tempo cards ...` via the launcher's `tempo-<name>` discovery. Covers Bridge customers/KYC/ToS, Stripe Issuing cards/cardholders/transactions/authorizations, and the on-chain USDC approval/allowance for the cards issuer.
- `crates/tempo-cards/src/main.rs` - CLI entry point
- `crates/tempo-cards/src/args.rs` - clap definitions (Cli + CardsCommands and subcommand enums)
- `crates/tempo-cards/src/app.rs` - Command dispatch and analytics tagging
- `crates/tempo-cards/src/commands/cards/` - Implementation (mod.rs, client.rs, config.rs, approval.rs)
- `crates/tempo-cards/tests/` - Integration tests (mocked Bridge + Stripe via axum)

#### `crates/tempo-request/` — package `tempo-request`, binary `tempo-request`

HTTP client with built-in MPP payment support. Source organized by module directories:
- `crates/tempo-request/src/main.rs` - CLI entry point
- `crates/tempo-request/src/args.rs` - clap definitions (Cli, QueryArgs)
- `crates/tempo-request/src/app.rs` - Command dispatch
- `crates/tempo-request/src/analytics.rs` - Request-specific analytics events and payloads
- `crates/tempo-request/src/query/` - Query flow (request prep, output, challenge parsing, SSE, analytics)
  - `mod.rs`, `analytics.rs`, `challenge.rs`, `headers.rs`, `output.rs`, `payload.rs`, `prepare.rs`, `sse.rs`
- `crates/tempo-request/src/http/` - HTTP client and request handling
  - `mod.rs`, `client.rs`, `fmt.rs`, `response.rs`
- `crates/tempo-request/src/payment/` - Payment flows (charge + session)
  - `mod.rs`, `charge.rs`, `router.rs`
  - `session/` - Session-based payment (flow.rs, open.rs, persist.rs, streaming.rs, voucher.rs)

#### `crates/tempo-sign/` — package `tempo-sign`, binary `tempo-sign`

Lightweight release manifest signing tool for authenticating build artifacts.
- `crates/tempo-sign/src/main.rs` - Signing tool source

**Packages:** `tempo-common`, `tempo-wallet`, `tempo-request`, `tempo-cards`, `tempo-sign`

## Commands

```bash
make build              # Build debug binary
make release            # Build optimized release binary
make test               # Run all tests (uses mocks, no network required)
make check              # Run fmt check, clippy, tests, and doc
make fix                # Auto-fix formatting and clippy warnings
make install            # Install CLI binaries to ~/.tempo/bin
make uninstall          # Uninstall CLI binaries
make run ARGS="<url>"   # Run tempo-wallet with arguments
```

## Agent Suggestions

When the user explicitly says "ask the oracle" to check a value, run `tempo-request` against OpenRouter and explicitly tell the user which model was used in the response.

Example:
```bash
tempo-request -v -X POST --json '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"what is 1+1"}]}'  https://openrouter.mpp.tempo.xyz/v1/chat/completions | jq
```

## CRITICAL: Pre-Commit Requirements

### Before Every Commit, You MUST:

1. ✅ **Check**: `make check` - ZERO issues

## Pull Request Guidelines

When creating pull requests:

1. **Always include the PR link** in your response after creating a PR
2. Format as a clickable link: `[#123](https://github.com/tempoxyz/wallet/pull/123)`
3. When creating multiple PRs, provide a summary table with all links

Example summary format:
```
| PR | Title | Link |
|----|-------|------|
| 1 | feat: add feature X | [#123](https://github.com/tempoxyz/wallet/pull/123) |
| 2 | fix: resolve issue Y | [#124](https://github.com/tempoxyz/wallet/pull/124) |
```

## Code Style Guidelines

### Rust Conventions

- **Edition**: Rust 2021
- **Error handling**: Prefer typed `TempoError` boundaries and source-carrying variants (`*Source`) where a concrete underlying error exists
- **Async runtime**: Tokio (minimal features: macros, rt-multi-thread, signal)
- **Serialization**: Serde with derive macros

### Imports

- Group imports: std → external crates → crate modules
- Use `use crate::` for internal module imports
- Use `use tempo_common::` for shared library imports

```rust
use std::path::PathBuf;

use clap::Parser;

use tempo_common::config::Config;
use tempo_common::error::TempoError;

fn run() -> Result<(), TempoError> {
    Ok(())
}
```

### Error Handling Pattern

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TempoError {
    #[error("failed to parse: {0}")]
    ParseError(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
```

### Module Organization

- Each module should have a clear single responsibility
- Use `mod.rs` for modules with submodules
- Shared logic goes in `crates/tempo-common/src/`
- All commands go in `crates/tempo-wallet/src/commands/`

### Testing

- Integration tests in `tests/` use assert_cmd for black-box CLI testing
- Use `TestConfigBuilder` for setting up test configurations
- Use `test_command(&temp)` helper to create properly configured CLI commands

```rust
use crate::common::{TestConfigBuilder, test_command};

#[test]
fn test_something() {
    let temp_dir = TestConfigBuilder::new().build();
    
    let mut cmd = test_command(&temp_dir);
    cmd.arg("--help");
    cmd.assert().success();
}
```

### CLI Patterns (clap)

- Use derive macros for argument parsing
- Flatten `GlobalArgs` from `tempo_common::cli` for shared flags
- Group related args with `help_heading`
- Support short aliases (`-v`) and long aliases (`--verbose`)

```rust
#[derive(Parser, Debug)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    
    #[command(subcommand)]
    pub command: Option<Commands>,
}
```

## Making Changes

### Before Starting Any Code Changes:
- [ ] Check existing patterns in similar files
- [ ] Identify affected tests

### Adding New Features

1. Add shared logic in `crates/tempo-common/src/`
2. Add CLI flags in the appropriate binary's `src/args.rs`
3. Implement commands in the appropriate binary's `src/commands/`
4. Add tests: unit tests in source files, integration tests in each crate's `tests/`

## Dependencies

### Key External Crates

| Crate | Purpose |
| ----- | ------- |
| `clap` | CLI argument parsing |
| `alloy` | EVM interactions and signing (minimal features) |
| `reqwest` | HTTP client |
| `serde` / `serde_json` / `toml` | Serialization |
| `tokio` | Async runtime (minimal features) |
| `mpp` | [Machine Payments Protocol](https://mpp.dev) SDK |

### Adding Dependencies

Add to `[workspace.dependencies]` in the root `Cargo.toml`, then reference with `dep.workspace = true` in the crate's `Cargo.toml`:

```toml
# Root Cargo.toml
[workspace.dependencies]
new-crate = "1.0"

# Crate Cargo.toml
[dependencies]
new-crate.workspace = true
```

## Environment Variables

| Variable | Description |
| -------- | ----------- |
| `TEMPO_HOME` | Override data directory (default: `~/.tempo`) |
| `TEMPO_RPC_URL` | Override RPC endpoint |
| `TEMPO_AUTH_URL` | Override auth server URL |
| `TEMPO_SERVICES_URL` | Override service directory API URL |
| `TEMPO_NO_TELEMETRY` | Disable telemetry |
| `TEMPO_PRIVATE_KEY` | Provide a private key directly for payment (bypasses wallet login and keychain; ephemeral) |
| `TEMPO_BRIDGE_API_KEY` / `BRIDGE_API_KEY` | Bridge API key for `tempo wallet cards customers ...` |
| `TEMPO_BRIDGE_API_URL` | Override Bridge API base URL for card tests/integration |
| `TEMPO_STRIPE_API_KEY` / `STRIPE_SECRET_KEY` / `STRIPE_API_KEY` | Stripe API key for `tempo wallet cards ...` Issuing commands |
| `TEMPO_STRIPE_API_URL` | Override Stripe API base URL for card tests/integration |

## Data Locations

All data lives under `$TEMPO_HOME` (default: `~/.tempo`):

```
~/.tempo/
├── config.toml              # Shared config (RPC overrides, telemetry)
└── wallet/
    ├── keys.toml             # Wallet keys (mode 0600)
    ├── cards.toml            # Bridge and Stripe API keys for wallet-backed cards (mode 0600)
    └── channels.db           # Persisted payment channel state (SQLite)
```

- Private keys: macOS Keychain (macOS) or inline in `keys.toml` (Linux)
- Card provider keys: env vars take precedence; saved keys live in `cards.toml` via `tempo wallet cards config ...`

## Configuration Structure

```rust
struct Config {
    tempo_rpc: Option<String>,    // Typed RPC override for Tempo mainnet
    moderato_rpc: Option<String>, // Typed RPC override for Moderato testnet
    rpc: HashMap<String, String>, // General RPC overrides by network id
}
```

**Wallet Fields (`keys.toml`):**
- `wallet_type` — `"local"` or `"passkey"`
- `wallet_address` — On-chain wallet address (the fundable address)
- `chain_id` — Chain ID this key is authorized for
- `key_type` — Signature type (`"secp256k1"`, `"p256"`, or `"webauthn"`)
- `key_address` — Address of the signing key
- `key` — Signing key private key stored inline; file is written with mode 0600
- `key_authorization` — RLP-encoded on-chain authorization proof for this key
- `expiry` — Unix timestamp for key authorization expiry
- `limits` — Array of `{ currency: "0x...", limit: "..." }`

**Key Selection:** Deterministic: passkey > first key with `key` > first key (lexicographically). The old `active` field was removed.

**Network Resolution Priority:**
1. `TEMPO_RPC_URL` env var (overrides everything)
2. Typed overrides (`tempo_rpc`, `moderato_rpc`) take precedence
3. General `[rpc]` table as fallback
4. Default RPC if no override

**Built-in Networks:** `tempo` (chain 4217, mainnet), `tempo-moderato` (chain 42431, testnet)

**Built-in Tokens:** USDC (mainnet), pathUSD (testnet)

## Documentation

- [Rust Book](https://doc.rust-lang.org/book/)
- [Alloy Documentation](https://alloy.rs/)
