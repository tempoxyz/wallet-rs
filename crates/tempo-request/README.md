# tempo-request

HTTP request extension for the Tempo CLI with built-in [MPP](https://mpp.dev) payment support. Makes HTTP requests and automatically handles `402 Payment Required` responses.

## Usage

```bash
# Make a request
tempo request https://api.example.com/data

# Make a paid API request
tempo request -X POST --json '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"Hello!"}]}' \
  https://openrouter.mpp.tempo.xyz/v1/chat/completions

# Preview cost without paying
tempo request --dry-run -X POST --json '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"Hello!"}]}' \
  https://openrouter.mpp.tempo.xyz/v1/chat/completions

# Select a payment intent when a server offers both
tempo request --payment-intent session https://api.example.com/data
tempo request --payment-intent charge https://api.example.com/data
```

`--payment-intent auto` is the default. It prefers a reusable session. If the session fails after
its extension attempt and the server also offered a compatible charge, the command reports the
exact one-time charge and requires an explicit retry with `--payment-intent charge`; it never
silently submits non-refundable charge capacity.

## License

Dual-licensed under [Apache 2.0](../../LICENSE-APACHE) and [MIT](../../LICENSE-MIT).
