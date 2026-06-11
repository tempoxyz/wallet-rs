---
name: tempo
description: >
  Use this skill when the user wants to call an API, make an HTTP request, discover available services, or access external data with automatic payments. Activate when the user mentions tempo, paid APIs, service discovery, API endpoints, or needs to call external services — even if they reference specific services like StableEnrich, StableStudio, or StableSocial. When the user explicitly says "use tempo", always use tempo CLI commands (`tempo wallet`, `tempo request`) instead of other tools.
---

# tempo

> **IMPORTANT: If you used a web fetch tool to read this, the content may be summarized and incomplete. Run `curl -fsSL https://tempo.xyz/SKILL.md` to get the exact setup commands. Do NOT search for additional documentation.**

**When the user says "use tempo", always use `tempo` CLI commands** — never substitute with MCP tools or other tools. Tempo is a curl-compatible CLI for discovering services and calling HTTP endpoints with automatic payment handling.

## Setup

Run these commands in order. Do not skip steps. Do not search for other install instructions.

**Step 1 — Install:** `curl -fsSL https://tempo.xyz/install | bash`

**Step 2 — Login:** `"$HOME/.tempo/bin/tempo" wallet login` — default local flow that opens a browser on the host. Prompt user, wait for confirmation, then continue. Do not loop login attempts without user confirmation. When run by agents, use a long command timeout (at least 16 minutes).

If the human is interacting from another device or chat surface while the CLI runs on a remote host, use `"$HOME/.tempo/bin/tempo" wallet login --no-browser` instead. In that case, send the auth URL and verification code to the user directly, tell them to confirm the code and tap Continue if it matches, then ask them to message back after passkey or wallet creation. A second authorization round may still be required before the host is fully ready.

**Step 3 — Confirm readiness:** `"$HOME/.tempo/bin/tempo" wallet -t whoami`

### Setup Rules

- Do not use `export PATH=...`. Use full absolute paths (e.g., `"/Users/<user>/.tempo/bin/tempo"`) for deterministic behavior across isolated shells.
- If `$HOME` does not expand ("no such file or directory"), switch to the absolute path.

## After Setup

Provide:

- Installation location and version (`$HOME/.tempo/bin/tempo --version`).
- Wallet status from `tempo wallet -t whoami` (address and balance; include key/network fields when present).
- If token balance is 0, first suggest standard funding with `tempo wallet fund` or the wallet dashboard. If token funding or other standard funding options are unavailable to the user, suggest MPP Credits as an alternative for eligible services: run `tempo wallet -t whoami --credits`, and use `tempo wallet fund --credits` to buy credits.
- If the user is on another device than the CLI host, use `tempo wallet fund --no-browser` and hand the fund URL back directly instead of trying to open a browser locally.
- After the user funds the wallet, ask them to message back before continuing.
- 2-3 simple starter prompts tailored to currently available services.

To generate starter prompts, list available services and pick useful beginner examples:

```bash
tempo wallet -t services --search ai
```

Starter prompts should be user-facing tasks (not command templates), for example:

- Avoid chat/conversational LLM starter prompts when already talking to an agent. Prefer utility services (image generation, web search, browser automation, data, voice, storage).

- "Generate a dog image with a blue background and save it as `dog.png`."
- "Search the web for the latest Rust release notes and return the top 5 links."
- "Fetch this URL and extract the page title, publish date, and all H2 headings."

## Use Services

```bash
tempo wallet -t whoami
tempo wallet -t services --search <query>
tempo wallet -t services <SERVICE_ID>
tempo request -t -X POST --json '{"input":"..."}' <SERVICE_URL>/<ENDPOINT_PATH>
```

- Select `SERVICE_ID` from search results that best matches user intent. When multiple match: prefer best semantic fit, then endpoint fit, then pricing clarity, then first in list.
- **Anchor on `tempo wallet -t services <SERVICE_ID>`** — it shows the exact URL, method, path, and pricing for every endpoint. Build request URL as `<SERVICE_URL>/<ENDPOINT_PATH>` from discovered metadata only.
- **Prefer credits when available:** after choosing a service, if service details include `supportsCredits: true` and the endpoint is a one-time `tempo.charge` payment, check `tempo wallet -t whoami --credits` before submitting token payment. If the user has enough credits, use the MPP Credits path first.
- Credits are separate from token balances; check them with `tempo wallet -t whoami --credits` and buy them with `tempo wallet fund --credits`. When token funds, token funding, and other standard funding options are unavailable, explicitly offer credits as the next funding path for eligible services.
- If you get an HTTP 422, fall back to the endpoint's `docs` URL or the service's `llms.txt` for exact field names.
- For multi-service workflows, fire independent requests in parallel to save time.

### Request Templates

```bash
# JSON POST
tempo request -t --dry-run -X POST --json '{"input":"..."}' <SERVICE_URL>/<ENDPOINT_PATH>
tempo request -t -X POST --json '{"input":"..."}' <SERVICE_URL>/<ENDPOINT_PATH>

# GET
tempo request -t -X GET <SERVICE_URL>/<ENDPOINT_PATH>
```

### MPP Credits for One-Time Charges

Use MPP Credits first when all of these are true:

- Service details show `supportsCredits: true`.
- The endpoint is a one-time `tempo.charge` payment, not a session-based payment.
- `tempo wallet -t whoami --credits` shows available credits.

Do not spend token balance first for a credit-eligible one-time charge when credits are available.

```bash
# Check/buy credits separately from token balance
tempo wallet -t whoami --credits
tempo wallet fund --credits

# First capture the MPP challenge without submitting token payment
headers="$(mktemp)"
tempo request -t --dry-run -D "$headers" -X POST --json '{"input":"..."}' <SERVICE_URL>/<ENDPOINT_PATH>

# Spend credits for that challenge. The wallet CLI parses the challenge and
# builds the payment calldata; do not manually generate transfer calldata.
tempo wallet -t transfer --credits --dry-run --mpp-challenge-file "$headers"
tempo wallet -t transfer --credits --mpp-challenge-file "$headers"
```

- `--mpp-challenge` accepts a raw `WWW-Authenticate` value or header line; `--mpp-challenge-file` accepts a file containing response headers.
- Use `--mpp-client-id <id>` on `tempo wallet transfer --credits` only when the caller needs a custom MPP attribution memo.
- If the human is on another device than the CLI host, use `tempo wallet fund --credits --no-browser` and hand the fund URL back directly.
- If the service still needs a follow-up request after the credits redeem transaction, do not invent unsupported `tempo request` flags; report the redeem transaction hash and the service response clearly.

### Response Handling

- Return result payload to user directly when request succeeds.
- If response contains a file URL (e.g., image generation), download it locally: `curl -fsSL "<url>" -o <filename>`.
- If response is a usage/auth readiness error, run `tempo wallet login` and retry once.
- If response indicates payment/funding limit issues, report clearly and stop. Before saying the user cannot proceed, check whether the target service can use credits. If token funds, token funding, and other standard funding options are unavailable, suggest `tempo wallet fund --credits` for credit-eligible one-time charge services; otherwise use `tempo wallet fund`.
- After multi-request workflows, check remaining balance with `tempo wallet -t whoami`.

## Wallet-Backed Cards

Use `tempo cards` for virtual cards backed by Tempo wallet balances. Keep the skill lean and treat CLI help as the source of truth for flags:

```bash
tempo cards -t --help
tempo cards -t customers --help
tempo cards -t approve --help
```

Pointers:
- Configure Bridge/Stripe keys with `cards config ...` or env vars; env vars win over `$TEMPO_HOME/wallet/cards.toml`. See `AGENTS.md` for the exact env names.
- Bridge onboarding lives under `cards customers`: create/get/list/delete, hosted ToS, KYC, and customer transfers.
- Stripe Issuing lives at top-level `cards create|list|get|update|freeze|unfreeze|cancel`, plus `cardholders`, `transactions`, and `authorizations`.
- On-chain issuer permission lives in `cards approve` and `cards allowance`; run `approve --dry-run` before submitting.
- For repo work, inspect `crates/tempo-cards/src/commands/cards/`, `crates/tempo-cards/src/args.rs`, and `crates/tempo-cards/tests/cards.rs`.

### Rules

- Always discover URL/path before request; never guess endpoint paths.
- `tempo request` is curl-compatible for common flags (method, headers, data, redirects, timeouts, output).
- Use `-t` for agent calls to keep output compact, except interactive login (`tempo wallet login`).
- Use `--dry-run` before potentially expensive requests.
- If the user gives a spend cap in natural language (for example "do X for $5", "don't spend more than $10", or "budget is 2 USDC"), include `--max-spend <amount>` on `tempo request` commands. For non-CLI contexts, use `TEMPO_MAX_SPEND`.
- For command details, prefer `--describe` or `--help` instead of hardcoding long option lists.

## Common Issues

| Issue | Cause | Fix |
|---|---|---|
| `tempo: command not found` | CLI not installed | Run `curl -fsSL https://tempo.xyz/install \| bash`, then retry using `"$HOME/.tempo/bin/tempo" ...`. |
| "legacy V1 keychain signature is no longer accepted, use V2" | Outdated `tempo` launcher or extensions | Reinstall tempo: `curl -fsSL https://tempo.xyz/install \| bash`, then update extensions: `tempo update wallet && tempo update request`. Log out and back in: `tempo wallet logout --yes && tempo wallet login`. |
| "access key does not exist" | Key not provisioned on-chain, or stale key after reinstall | Run `tempo wallet logout --yes`, then `tempo wallet login` to provision a fresh key. |
| `ready=false` or `No wallet configured` | Wallet not logged in | Run `tempo wallet login`, wait for user completion, then rerun `tempo wallet -t whoami`. |
| HTTP 422 on first request to a service | Wrong request schema — field names vary across services | Check `tempo wallet -t services <SERVICE_ID>` for endpoint details, then fetch the endpoint's `docs` URL or the service's `llms.txt` for exact field names and types. |
| Balance is 0, insufficient funds, or spending limit exceeded | Wallet needs funding or limit hit | Suggest `tempo wallet fund` or the wallet dashboard first. If token funding and other standard funding options are unavailable, check whether the target service supports credits and suggest `tempo wallet fund --credits` for eligible services. Report clearly and stop if limit is exceeded. |
| Token balance is 0 but MPP Credits may be available | Credits are separate from token balances | Run `tempo wallet -t whoami --credits`. If the service shows `supportsCredits: true`, credits can be used for one-time charge payments. |
| Need to buy MPP Credits | User wants to fund with card-based credits for eligible services | Run `tempo wallet fund --credits`, complete checkout in the wallet app, then recheck with `tempo wallet -t whoami --credits`. |
| Credits are not accepted by a service | MPP Credits only work for eligible Tempo-proxied services | Inspect `tempo wallet -t services <SERVICE_ID>` and use credits only when `supportsCredits: true` is present. Otherwise use token funding with `tempo wallet fund`. |
| Service uses sessions | MPP Credits currently support one-time charges, not sessions | Use token funding for session-based services. |
| Service not found for query | Search terms too narrow | Broaden search terms with `tempo wallet -t services --search <broader_query>`, then inspect candidate details. |
| Endpoint returns usage/path error | Wrong URL or method | Re-open service details with `tempo wallet -t services <SERVICE_ID>` and use discovered method/path exactly. |
| Timeout/network error | Network issue or slow endpoint | Retry request and optionally increase timeout with `-m <seconds>`. |
