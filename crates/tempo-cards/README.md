# tempo-cards

Wallet-backed cards extension for the Tempo CLI. Invoked as `tempo cards ...` via the launcher's `tempo-<name>` discovery, or directly as `tempo-cards ...`.

Provides:

- **Bridge customer onboarding** — create customers, hosted ToS / KYC links, transfer history.
- **Stripe Issuing** — create / list / retrieve / update / freeze / unfreeze / cancel virtual cards, cardholders, transactions, and authorizations.
- **On-chain approval** — approve the card issuer to spend wallet USDC on Tempo (`approve` / `allowance`).

## Commands

| Command | Description |
| ------- | ----------- |
| `tempo cards config bridge-api-key <key>` | Save Bridge API key |
| `tempo cards config stripe-api-key <key>` | Save Stripe API key |
| `tempo cards config show` | Show current card configuration |
| `tempo cards customers create -f -l -e` | Create a Bridge customer |
| `tempo cards customers tos-acceptance-link <id>` | Hosted ToS link |
| `tempo cards customers kyc-link <id> --endorsement cards` | Hosted KYC link |
| `tempo cards customers get/list/delete/transfers` | Manage customers |
| `tempo cards create --cardholder <id>` | Issue a virtual card backed by your wallet |
| `tempo cards list/get/update/freeze/unfreeze/cancel` | Manage cards |
| `tempo cards cardholders list/get` | Manage cardholders |
| `tempo cards transactions list/get` | Manage transactions |
| `tempo cards authorizations list/get` | Manage authorizations |
| `tempo cards approve --amount <USDC \| max>` | Approve issuer to spend wallet USDC |
| `tempo cards allowance` | Show current issuer allowance |

## Configuration

API keys can come from env vars or be persisted to `~/.tempo/wallet/cards.toml` (mode 0600) via `tempo cards config`.

| Variable | Purpose |
| -------- | ------- |
| `TEMPO_BRIDGE_API_KEY` / `BRIDGE_API_KEY` | Bridge API key |
| `TEMPO_BRIDGE_API_URL` | Override Bridge API base URL (testing) |
| `TEMPO_STRIPE_API_KEY` / `STRIPE_SECRET_KEY` / `STRIPE_API_KEY` | Stripe API key |
| `TEMPO_STRIPE_API_URL` | Override Stripe API base URL (testing) |

## Example end-to-end flow

```bash
tempo cards config bridge-api-key sk-test-...
tempo cards config stripe-api-key sk_test_...

tempo cards customers create -f John -l Doe -e john@example.com
tempo cards customers tos-acceptance-link <bridge-customer-id>
tempo cards customers kyc-link <bridge-customer-id> --endorsement cards

tempo cards create \
  --cardholder <stripe-cardholder-id> \
  --bridge-customer-id <bridge-customer-id>

tempo cards approve --amount max
```

## License

Dual-licensed under [Apache 2.0](../../LICENSE-APACHE) and [MIT](../../LICENSE-MIT).
