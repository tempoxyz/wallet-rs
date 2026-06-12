# Changelog

## 0.5.2 (2026-06-12)

### Patch Changes

- Make the Tempo skill credits flow easier to discover by surfacing credit usage and first-time funding guidance more prominently. (by @alexrisch, [#501](https://github.com/tempoxyz/wallet/pull/501))

## 0.5.1 (2026-06-10)

### Patch Changes

- Update the credits funding handoff URL to use the fund action with a credits intent. (by @alexrisch, [#499](https://github.com/tempoxyz/wallet/pull/499))

## 0.5.0 (2026-06-10)

### Minor Changes

- Added TIP-1034 Tempo session client support, including descriptor-backed channel persistence, session reuse, top-ups, SSE voucher updates, and cooperative close handling while preserving legacy session compatibility. (by @alexrisch, [#494](https://github.com/tempoxyz/wallet/pull/494))

### Patch Changes

- Exposed service credit support in wallet service discovery output, allowed filtering for credit-capable services, fixed credits transfer dry runs to avoid redeem submission, and documented the MPP Credits workflow. (by @alexrisch, [#495](https://github.com/tempoxyz/wallet/pull/495), [#496](https://github.com/tempoxyz/wallet/pull/496))

## 0.4.5 (2026-06-04)

### Minor Changes

- Add wallet CLI support for checking and spending Coinflow credits, and update shared signer handling so wallet flows can emit the expected Tempo signature format for these transactions. (by @alexrisch, [#471](https://github.com/tempoxyz/wallet/pull/471))

## 0.4.4 (2026-06-03)

### Patch Changes

- Fix v0.4.3 wallet regressions by sending auth `chainId` in the backend-compatible hex/camelCase shape and by preventing transient key spending-limit preflight failures from forcing an already-provisioned access key through registration again. (by @Kartik, [#485](https://github.com/tempoxyz/wallet/pull/485))
- Pin the `mpp` SDK to tempoxyz/mpp-rs@8ba7631 while the 0.10.5 publish is pending.

## 0.4.3 (2026-06-02)

### Patch Changes

- Add the standalone `tempo-cards` extension for wallet-backed card configuration, customer management, approvals, and provider-backed card workflows. (by @letstokenize, [#480](https://github.com/tempoxyz/wallet/pull/480))
- Preserve both halves of an oversized session channel log scan when an RPC requires a smaller `eth_getLogs` range, avoiding skipped upper-half ranges during wallet session recovery. (by @EfeBaranDurmaz, [#462](https://github.com/tempoxyz/wallet/pull/462))
- Pass the selected login network through wallet authentication requests so testnet CLI logins stay on testnet instead of falling back to mainnet browser state. (by @BrendanRyan, [#456](https://github.com/tempoxyz/wallet/pull/456))
- Fixed `tempo request` failing on 402 responses that offer multiple `tempo`-method payment challenges (e.g. moderato + mainnet on the same endpoint). The CLI now decodes every offered challenge and picks the first one the wallet can actually satisfy — matching `--network` (when set) and the keystore's `(chain_id, currency)`. When no challenge matches, a new `NoCompatibleChallenge` error lists the offered and held options and suggests `tempo wallet fund` / `tempo wallet login` instead of failing later with a cryptic "No key configured" message. (by @BrendanRyan, [#477](https://github.com/tempoxyz/wallet/pull/477))
- Redact query parameters from paid-success analytics events so payment flows do not report URL secrets after a 402 challenge succeeds. (by @DimitryFiuse, [#458](https://github.com/tempoxyz/wallet/pull/458))

## 0.4.2 (2026-05-04)

### Patch Changes

- Bump posthog-rs 0.5.0 → 0.5.3 (drops reqwest 0.11/rustls 0.21 chain), remove stale advisory ignores, add dependabot update grouping.
- Surface access-key spending limit reverts returned by Tempo RPC as a specific wallet error with guidance to refresh or re-authorize the spending limit.
- Preserve both the MPP client attribution ID and the configured signing mode when handling zero-amount charge payments.
- Pin all GitHub Actions to commit SHAs, fix template injection in CI workflows, scope permissions per-job, replace curl|sh with checksum-verified binary download, add Dependabot cooldown, and suppress unfixable transitive advisories in deny.toml.

## 0.4.1 (2026-04-10)

### Patch Changes

- Pass `signing_mode` to `TempoProvider` in the zero-amount charge path, matching the paid charge path. Without this, the provider defaults to Direct mode and ignores keychain configuration.

## 0.4.0 (2026-04-07)

### Minor Changes

- Added zero-amount proof credential support for identity flows. When a server issues a charge challenge with `amount="0"`, the wallet now signs an EIP-712 proof credential instead of building an on-chain transaction, enabling authentication without moving funds. Upgraded `mpp` SDK to v0.9.0.

## Unreleased

### Patch Changes

- Fix gas estimation failure when `feePayer: true` by using the real fee token address for `eth_estimateGas` instead of the zero address. The final signed transaction still uses `Address::ZERO` for server-side sponsor co-signing.

## 0.3.0 (2026-03-31)

### Minor Changes

- Upgraded `mpp` SDK to v0.8.3, adding support for split payments. Charges with `splits` in the 402 challenge are now handled transparently via multi-transfer AA transactions.

## 0.2.2 (2026-03-27)

### Patch Changes

- Add a remote-host-friendly `--no-browser` path for `tempo wallet login` and `tempo wallet fund`, including CLI guidance that agents can relay to a user approving wallet actions from another device.

## 0.2.1 (2026-03-26)

### Patch Changes

- Bump to the latest `mpp-rs` `main`, including upstream security fixes for payment bypass, replay, fee-payer manipulation, and session/channel griefing vulnerabilities across Tempo and Stripe flows. The update also refreshes the Tempo client dependency graph and switches Tempo gas estimation to a request-based API while preserving existing wallet/request behavior.
- Stabilize voucher HEAD fallback tests by serializing cases that share a process-global unsupported-origin cache.
- Preserve non-2xx response bodies when fetching the service directory so CLI errors include upstream details instead of only the HTTP status.
- Add `chain_id` to `tempo wallet transfer` output and render submitted transaction hashes as explorer hyperlinks, with a plain URL fallback when terminal hyperlinks are unsupported.

## 0.2.0 (2026-03-24)

### Minor Changes

- Add `tempo wallet refresh` to refresh passkey/access-key authorization without a full relogin, including improved login/refresh command flow and validation messaging for expired or stale authorizations.
- Enforce strict session `Payment-Receipt` handling across all session flows, including reused persisted sessions that were previously permissive: reject successful paid responses that omit or malformedly encode receipts, require valid `spent` semantics for response/header/event receipts, preserve conservative local channel state when strict top-up receipt validation fails after a paid response, and extend integration coverage for strict open/top-up/streaming receipt failure paths.

### Patch Changes

- Add `--max-spend` / `TEMPO_MAX_SPEND` hard cap for cumulative session spend, enforced at challenge time, on session reuse, at channel open, and during streaming top-ups. Also reconcile on-chain channel state after cooperative-close 5xx failures before falling back to payer-side close, and fix session reuse to reject candidates from a different origin.
- Track server-reported `Payment-Receipt.spent` for session state and use it as the close target instead of the payer-signed cumulative ceiling. This adds persisted `server_spent` support and updates request/session close flows so cooperative close settles the amount the server actually reports as spent.
- Fix session top-up which was completely broken due to five compounding bugs: ABI mismatch (topUp used uint128 instead of uint256 producing wrong function selector), voucher cumulative amount was incorrectly clamped to available balance preventing top-up from ever triggering, AmountExceedsDeposit problem type was not handled alongside InsufficientBalance, stale challenge echo was used for top-up requests causing server rejection, and missing requiredTopUp field in server response caused a hard failure instead of computing the value from local state. Also signals ChannelInvalidated when on-chain top-up fails so the caller can re-open a new channel.
- Added `help_heading = "Network"` to the `--network` CLI argument for improved help output organization.
- Only purge local credentials/session state for inactive access-key errors after confirming key state on-chain. This prevents destructive cleanup on transient or misclassified failures and improves error classification for charge/session payment paths.
- Fix hyperlink sanitization tests to correctly extract and validate only the display text portion of OSC 8 hyperlink sequences, preventing false failures in OSC 8-capable terminals.
- Harden paid session SSE handling for real-world providers by improving retry/stream behavior and session state synchronization across open, voucher, and streaming flows. This reduces false failures and makes strict payment processing more resilient to provider response quirks.
- Improve `tempo wallet sessions close` output by printing a blank line before per-channel progress logs for better readability during multi-session close operations.
- Slim the service list schema to replace full endpoint details with an `endpoint_count` field, reducing payload size. Adds a test to enforce the summary-only structure.

## 0.1.5 (2026-03-18)

### Patch Changes

- Remove hardcoded credentials and tokens from default RPC URLs, auth URLs, and services API URL. Unhide the `--network` CLI flag and update its help text. Improve changelog generation by embedding full format instructions in the AI prompt.
- Adds an `accepted_cumulative` field to `ChannelRecord` to track the server-confirmed accepted amount separately from the payer-signed ceiling (`cumulative_amount`). Cooperative close now uses the accepted amount instead of the signing ceiling to avoid overcharging the payer, with a DB migration and monotonic update logic throughout the storage and request layers.
- Fix cooperative session close by selecting the correct WWW-Authenticate challenge when the proxy returns multiple headers (charge and session intents). Parse problem+json error details from close failures to surface actionable messages instead of generic errors.
- Improve close command progress output: add status messages when closing local sessions, sessions by URL, and orphaned channels, and refactor `finalize_closed_channels` to use iterator filtering with a count message before finalizing.

## Unreleased

### Patch Changes

- Improve open-source readiness across docs and metadata: fix contributor setup instructions, replace dead example links, add a security policy document, and refresh the root README structure.
- Remove embedded routing/rate-limit tokens from built-in Tempo default RPC and wallet auth URLs, using token-free base endpoints instead.

## 0.1.4 (2026-03-18)

### Patch Changes

- Tighten charge payment provisioning retry to only fire on auth/payment (401–403) and server error (5xx) status codes, avoiding wasteful retries on unrelated API errors like 400 body validation. Show full server response body in payment rejection errors instead of extracting a single JSON field. Ensure all retry paths surface the original error on retry failure.

## 0.1.4 (2026-03-18)

## 0.1.3 (2026-03-18)

### Patch Changes

- Fix charge payment failing with "access key does not exist" when the signing key is not yet provisioned on-chain. The server-side rejection retry only triggered on HTTP 401-403, but the server returns other status codes for keychain errors.

## 0.1.3 (2026-03-18)

### Patch Changes

- Fix charge payment failing with "access key does not exist" when the signing key is not yet provisioned on-chain. The server-side rejection retry only triggered on HTTP 401-403, but the server returns other status codes for keychain errors.

## 0.1.2 (2026-03-18)

### Patch Changes

- Persist channel state when channel open fails to prevent orphaned on-chain funds.
- When the open transaction is sent to the server but the server returns an error, the channel may already exist on-chain. Previously the channel state was lost, making the deposited funds unrecoverable. Now the channel record is persisted before returning the error, so future runs can discover and close it.
- Fix session deposit and key-auth retry error handling.
- Preserve original error when key-authorization retry also fails, instead of showing misleading retry errors (e.g. `KeyAlreadyExists`).
- Ensure session deposit covers at least the per-request cost, preventing channels from opening with insufficient funds.
- Fail early before opening a channel if wallet balance is too low to cover the request cost.

## 0.1.2 (2026-03-18)

## 0.1.1 (2026-03-18)

### Patch Changes

- Removed the "all" amount option from the transfer command, along with the associated `query_balance` helper and `balanceOf` interface method. The `resolve_amount` function is now synchronous.
- Migrated the `fund` command to a browser-based flow, replacing the previous faucet/bridge implementations with a simple browser-open + balance polling approach. Added network name aliases (`mainnet`, `testnet`, `moderato`) for `NetworkId` parsing, removed the `qrcode` dependency, updated install paths from `~/.local/bin` to `~/.tempo/bin`, and removed `--no-wait` and `--dry-run` flags from the fund command.

## 0.1.1 (2026-03-17)

### Patch Changes

- Simplified optimistic key provisioning by removing the `query_key_status` and `prepare_provisioning_retry` functions, instead retrying directly with `with_key_authorization()` on any error. Added support for merged `WWW-Authenticate` challenges (RFC 9110 §11.6.1) by splitting and selecting the first supported payment method. Fixed `list_channels` to exclude localhost origins and removed the realm-vs-origin validation check.
