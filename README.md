<br>
<br>

<p align="center">
  <a href="https://tempo.xyz">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/tempoxyz/tempo/refs/heads/main/.github/assets/tempo-wordmark-white.svg">
      <img alt="Tempo wordmark" src="https://raw.githubusercontent.com/tempoxyz/tempo/refs/heads/main/.github/assets/tempo-wordmark-black.svg" width="360">
    </picture>
  </a>
</p>

<br>
<br>

# Tempo Wallet

**Command-line wallet and HTTP client for the [Tempo](https://tempo.xyz) blockchain, with built-in [Machine Payments Protocol](https://mpp.dev) support.**

**[Website](https://wallet.tempo.xyz)**
| [Docs](https://docs.tempo.xyz/cli)
| [MPP Spec](https://mpp.dev)
| [Architecture](ARCHITECTURE.md)

## What is Tempo Wallet?

Tempo Wallet is a CLI that lets you create a wallet, manage keys, and make HTTP requests that pay automatically — no API keys required. It uses the [Machine Payments Protocol (MPP)](https://mpp.dev) to handle `402 Payment Required` challenges natively, turning any paid API into a simple HTTP call.

### How Login Works

1. Run `tempo wallet login` — the CLI opens your browser to [wallet.tempo.xyz](https://wallet.tempo.xyz).
2. Authenticate with your passkey (Touch ID, Face ID, or hardware key).
3. The browser authorizes a session key for the CLI and redirects back.
4. The CLI stores the authorized key locally. All subsequent signing happens locally — no browser needed.

If the agent is running on a remote host while you are using a different device, use `tempo wallet login --no-browser` instead. The CLI will print the auth URL and verification code for you to open on your device, and you may need to return to your CLI or agent session after passkey creation or after funding. A second authorization round may still be needed before the host is ready.

## Goals

1. **Zero-config payments**: `tempo request <url>` handles the full 402 flow — challenge, sign, pay, retry — in a single command.
2. **Secure by default**: Passkey login with scoped session keys — time-limited, spending-capped, and chain-bound. Your passkey never leaves the browser; the CLI only holds a restricted access key.
3. **Composable**: Both `tempo-wallet` and `tempo-request` are standalone binaries that the [`tempo` CLI](https://github.com/tempoxyz/tempo) discovers as extensions. Use them independently or together.
4. **Streaming-native**: Session-based payments support SSE streaming with per-token voucher top-ups — pay only for what you consume.

## Install

```bash
curl -fsSL https://tempo.xyz/install | bash
```

This installs the `tempo` launcher, which automatically manages wallet extensions.

### Install Skill

```bash
npx skills@latest add tempoxyz/wallet --global
```

## Quick Start

```bash
# Log in with your passkey (opens browser)
tempo wallet login

# Remote-host login when the human is on another device
tempo wallet login --no-browser

# Check your wallet
tempo wallet whoami

# Fund your wallet
tempo wallet fund

# Remote-host funding when the human is on another device
tempo wallet fund --no-browser
```

### One-Shot Payment (Charge)

Every request is independently settled on-chain. No sessions, no state.

```bash
# Preview the cost (dry run)
tempo request --dry-run \
  https://aviationstack.mpp.tempo.xyz/v1/flights?flight_iata=AA100

# Make the request
tempo request \
  https://aviationstack.mpp.tempo.xyz/v1/flights?flight_iata=AA100
```

### Session Payment (Channel)

A session opens an on-chain channel once, then exchanges off-chain vouchers for subsequent requests — ideal for streaming and repeated calls.

```bash
# Make a session-based request (channel opens automatically)
tempo request -X POST \
  --json '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"Hello!"}]}' \
  https://openrouter.mpp.tempo.xyz/v1/chat/completions

# List active sessions
tempo wallet sessions list

# Close the session and settle on-chain
tempo wallet sessions close https://openrouter.mpp.tempo.xyz
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and workflow.

```bash
make build    # Build debug binaries
make test     # Run all tests
make check    # Format, clippy, test, docs
```

The Minimum Supported Rust Version (MSRV) is specified in [`Cargo.toml`](Cargo.toml).

## Security

See [SECURITY.md](SECURITY.md) for vulnerability reporting and for instructions on [verifying release binaries](SECURITY.md#verifying-releases) (SLSA provenance, SBOM, Sigstore signatures, and SHA-256 checksums).

## License

Dual-licensed under [Apache 2.0](LICENSE-APACHE) and [MIT](LICENSE-MIT).
