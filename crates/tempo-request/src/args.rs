//! CLI argument definitions and parsing.

use clap::{Parser, ValueEnum};

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

/// Payment intent selected from a multi-challenge response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum PaymentIntent {
    /// Prefer a session and require explicit consent before charge fallback.
    #[default]
    Auto,
    /// Use only a one-time charge challenge.
    Charge,
    /// Use only a reusable session challenge.
    Session,
}

impl PaymentIntent {
    /// Stable command-line value used in diagnostics.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Charge => "charge",
            Self::Session => "session",
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "tempo request")]
#[command(about = "Make HTTP requests with automatic MPP payment", long_about = None)]
#[command(version = LONG_VERSION)]
#[command(override_usage = "\n  tempo request [OPTIONS] <URL>")]
#[command(after_help = "\
Examples:
  tempo request https://api.example.com/data
  tempo request -X POST --json '{\"prompt\":\"hello\"}' https://api.example.com/v1/chat
  tempo request -H 'Accept: text/plain' -o out.txt https://api.example.com/data")]
pub(crate) struct Cli {
    #[command(flatten)]
    pub query: QueryArgs,

    #[command(flatten)]
    pub global: tempo_common::cli::GlobalArgs,
}

/// HTTP request arguments with optional payment support.
#[derive(Parser, Debug, Default)]
pub(crate) struct QueryArgs {
    /// URL to request
    #[arg(value_name = "URL")]
    pub url: String,

    /// Dry run mode - show what would be paid without executing
    #[arg(long, help_heading = "Payment Options")]
    pub dry_run: bool,

    /// Hard cap for cumulative payment spend (can also be set via TEMPO_MAX_SPEND)
    #[arg(
        long = "max-spend",
        value_name = "AMOUNT",
        env = "TEMPO_MAX_SPEND",
        help_heading = "Payment Options"
    )]
    pub max_spend: Option<String>,

    /// Payment intent for multi-challenge responses
    #[arg(
        long = "payment-intent",
        value_enum,
        default_value_t,
        help_heading = "Payment Options"
    )]
    pub payment_intent: PaymentIntent,

    /// Offline mode - fail immediately without making any network requests
    #[arg(long, help_heading = "HTTP Options")]
    pub offline: bool,

    /// Include HTTP headers in output
    #[arg(short = 'i', long = "include", help_heading = "Display Options")]
    pub include_headers: bool,

    /// Write output to file
    #[arg(
        short = 'o',
        long = "output",
        value_name = "FILE",
        help_heading = "Display Options"
    )]
    pub output: Option<String>,

    /// Custom request method
    #[arg(
        short = 'X',
        long = "request",
        value_name = "METHOD",
        help_heading = "HTTP Options"
    )]
    pub method: Option<String>,

    /// Shorthand for HEAD request (fetch headers only)
    #[arg(short = 'I', help_heading = "HTTP Options")]
    pub head: bool,

    /// Add custom header
    #[arg(
        short = 'H',
        long = "header",
        value_name = "HEADER",
        help_heading = "HTTP Options"
    )]
    pub headers: Vec<String>,

    /// Follow redirects (disabled by default)
    #[arg(short = 'L', long = "location", help_heading = "HTTP Options")]
    pub location: bool,

    /// Send data as query parameters with GET
    #[arg(short = 'G', long = "get", help_heading = "HTTP Options")]
    pub get: bool,

    /// Maximum time for the request in seconds
    #[arg(
        short = 'm',
        long = "timeout",
        value_name = "SECONDS",
        help_heading = "HTTP Options"
    )]
    pub max_time: Option<u64>,

    /// Maximum time to establish the TCP connection in seconds
    #[arg(
        long = "connect-timeout",
        value_name = "SECONDS",
        help_heading = "HTTP Options"
    )]
    pub connect_timeout: Option<u64>,

    /// POST data (use @filename to read from file, @- to read from stdin)
    #[arg(
        short = 'd',
        long = "data",
        value_name = "DATA",
        help_heading = "HTTP Options"
    )]
    pub data: Vec<String>,

    /// Send JSON data with Content-Type header
    #[arg(long = "json", value_name = "JSON", help_heading = "HTTP Options")]
    pub json: Option<String>,

    /// Send TOON data (decoded to JSON) with Content-Type header
    #[arg(
        long = "toon",
        value_name = "TOON",
        help_heading = "HTTP Options",
        conflicts_with = "json"
    )]
    pub toon: Option<String>,

    /// Number of retries on transient network errors (timeouts/connect failures)
    #[arg(long = "retries", value_name = "N", help_heading = "HTTP Options")]
    pub retries: Option<u32>,

    /// Initial retry backoff in milliseconds (doubles each retry, capped)
    #[arg(
        long = "retry-backoff",
        value_name = "MILLIS",
        help_heading = "HTTP Options"
    )]
    pub retry_backoff_ms: Option<u64>,

    /// Allow insecure TLS (skip certificate validation)
    #[arg(short = 'k', long = "insecure", help_heading = "HTTP Options")]
    pub insecure: bool,

    /// Override the User-Agent header
    #[arg(
        short = 'A',
        long = "user-agent",
        value_name = "STRING",
        help_heading = "HTTP Options"
    )]
    pub user_agent: Option<String>,

    /// Write response headers to a file
    #[arg(
        short = 'D',
        long = "dump-header",
        value_name = "FILE",
        help_heading = "HTTP Options"
    )]
    pub dump_header: Option<String>,

    /// Provide HTTP Basic auth credentials (user:pass)
    #[arg(
        short = 'u',
        long = "user",
        value_name = "USER:PASS",
        help_heading = "HTTP Options"
    )]
    pub user: Option<String>,

    /// Stream response body as it arrives
    #[arg(long = "stream", help_heading = "HTTP Options")]
    pub stream: bool,

    /// Treat response as Server-Sent Events and pass through
    #[arg(long = "sse", help_heading = "HTTP Options")]
    pub sse: bool,

    /// Treat response as SSE and output each event as NDJSON
    #[arg(long = "sse-json", help_heading = "HTTP Options")]
    pub sse_json: bool,

    /// Retry on these HTTP status codes (comma-separated list)
    #[arg(
        long = "retry-http",
        value_name = "CODES",
        help_heading = "HTTP Options"
    )]
    pub retry_http: Option<String>,

    /// Respect Retry-After header for retry delays
    #[arg(long = "retry-after", help_heading = "HTTP Options")]
    pub retry_after: bool,

    /// Add jitter percentage to retry backoff
    #[arg(
        long = "retry-jitter",
        value_name = "PCT",
        help_heading = "HTTP Options"
    )]
    pub retry_jitter: Option<u32>,

    /// Authorization bearer token (alternative to -u Basic auth)
    #[arg(long = "bearer", hide_env_values = true, help_heading = "HTTP Options")]
    pub bearer: Option<String>,

    /// Write response metadata (JSON) to file
    #[arg(
        long = "write-meta",
        value_name = "FILE",
        help_heading = "HTTP Options",
        hide = true
    )]
    pub write_meta: Option<String>,

    /// Use an HTTP/HTTPS proxy
    #[arg(long = "proxy", value_name = "URL", help_heading = "HTTP Options")]
    pub proxy: Option<String>,

    /// Disable all proxy use
    #[arg(long = "no-proxy", help_heading = "HTTP Options")]
    pub no_proxy: bool,

    /// Maximum redirects when -L is used
    #[arg(long = "max-redirs", value_name = "N", help_heading = "HTTP Options")]
    pub max_redirs: Option<u32>,

    /// Enable HTTP/2 (ALPN)
    #[arg(
        long = "http2",
        help_heading = "HTTP Options",
        conflicts_with = "http1_1"
    )]
    pub http2: bool,

    /// Force HTTP/1.1 only
    #[arg(
        long = "http1.1",
        visible_alias = "http1_1",
        help_heading = "HTTP Options",
        conflicts_with = "http2"
    )]
    pub http1_1: bool,

    /// Set the Referer header
    #[arg(
        short = 'e',
        long = "referer",
        value_name = "URL",
        help_heading = "HTTP Options"
    )]
    pub referer: Option<String>,

    /// Request a compressed response
    #[arg(long = "compressed", help_heading = "HTTP Options")]
    pub compressed: bool,

    /// Save output to a file named after the URL's last path segment
    #[arg(short = 'O', long = "remote-name", help_heading = "HTTP Options")]
    pub remote_name: bool,

    /// URL-encode a data field (repeatable)
    #[arg(
        long = "data-urlencode",
        value_name = "DATA",
        help_heading = "HTTP Options"
    )]
    pub data_urlencode: Vec<String>,

    /// Multipart form field (name=value, name=@file, name=@file;type=mime)
    #[arg(
        short = 'F',
        long = "form",
        value_name = "FIELD",
        help_heading = "HTTP Options",
        conflicts_with_all = ["data", "data_urlencode", "json", "toon", "get"]
    )]
    pub form: Vec<String>,
}

impl QueryArgs {
    /// Whether the request should use streaming mode (raw, SSE passthrough, or SSE→NDJSON).
    pub(crate) const fn is_streaming(&self) -> bool {
        self.stream || self.sse || self.sse_json
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_intent_defaults_to_auto() {
        let cli = Cli::try_parse_from(["tempo-request", "https://example.com"]).unwrap();
        assert_eq!(cli.query.payment_intent, PaymentIntent::Auto);
    }

    #[test]
    fn payment_intent_accepts_all_modes() {
        let modes = ["auto", "session", "charge"]
            .map(|mode| {
                Cli::try_parse_from([
                    "tempo-request",
                    "--payment-intent",
                    mode,
                    "https://example.com",
                ])
                .unwrap()
                .query
                .payment_intent
                .as_str()
            })
            .join(",");
        assert_eq!(modes, "auto,session,charge");
    }
}
