//! HTTP request planning, client building, execution, and retry logic.

use std::time::Duration;

use tracing::warn;

use super::response::HttpResponse;
use tempo_common::{
    error::{ConfigError, NetworkError, TempoError},
    network::NetworkId,
};

type HttpResult<T> = std::result::Result<T, TempoError>;

/// Default User-Agent header value for requests.
pub(crate) const DEFAULT_USER_AGENT: &str = concat!("tempo/", env!("CARGO_PKG_VERSION"));

/// A single field in a multipart form body.
#[derive(Debug, Clone)]
pub(crate) enum MultipartField {
    Text {
        name: String,
        value: String,
    },
    File {
        name: String,
        filename: String,
        content_type: Option<String>,
        bytes: Vec<u8>,
    },
}

/// Replayable HTTP request body.
///
/// Multipart bodies cannot use `reqwest::multipart::Form` directly because
/// `Form` is not `Clone`. Instead we store the field specs and rebuild a
/// fresh `Form` on every send (initial, retry, and payment replay).
#[derive(Debug, Clone)]
pub(crate) enum HttpRequestBody {
    Bytes(Vec<u8>),
    Multipart(Vec<MultipartField>),
}

/// Pre-resolved HTTP request plan, independent of CLI types.
#[derive(Debug)]
pub(crate) struct HttpRequestPlan {
    pub(crate) method: reqwest::Method,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Option<HttpRequestBody>,
    pub(crate) timeout_secs: Option<u64>,
    pub(crate) connect_timeout_secs: Option<u64>,
    pub(crate) follow_redirects: bool,
    pub(crate) follow_redirects_limit: Option<usize>,
    pub(crate) user_agent: String,
    pub(crate) insecure: bool,
    pub(crate) proxy: Option<String>,
    pub(crate) no_proxy: bool,
    pub(crate) http2: bool,
    pub(crate) http1_only: bool,
    // Retry configuration
    pub(crate) max_retries: u32,
    pub(crate) base_backoff_ms: u64,
    pub(crate) max_backoff_ms: u64,
    pub(crate) retry_status_codes: Vec<u16>,
    pub(crate) honor_retry_after: bool,
    pub(crate) retry_jitter_pct: Option<u32>,
}

impl Default for HttpRequestPlan {
    fn default() -> Self {
        Self {
            method: reqwest::Method::GET,
            headers: vec![],
            body: None,
            timeout_secs: None,
            connect_timeout_secs: None,
            follow_redirects: false,
            follow_redirects_limit: None,
            user_agent: DEFAULT_USER_AGENT.to_string(),
            insecure: false,
            proxy: None,
            no_proxy: false,
            http2: false,
            http1_only: false,
            max_retries: 0,
            base_backoff_ms: 0,
            max_backoff_ms: 0,
            retry_status_codes: vec![],
            honor_retry_after: false,
            retry_jitter_pct: None,
        }
    }
}

/// HTTP client with connection pooling, retry logic, and runtime configuration.
///
/// Owns a pre-built reqwest client and the request plan. Built at the CLI
/// boundary so that HTTP and payment modules never depend on CLI types.
pub(crate) struct HttpClient {
    plan: HttpRequestPlan,
    client: reqwest::Client,
    pub(crate) verbosity: tempo_common::cli::Verbosity,
    pub(crate) network: Option<NetworkId>,
    pub(crate) dry_run: bool,
    pub(crate) max_spend: Option<String>,
}

impl HttpClient {
    /// Build an `HttpClient`, constructing the reqwest client from the plan.
    ///
    /// Bakes transport-level settings (timeouts, TLS, proxy, redirects,
    /// default headers) into the client. Per-request headers (e.g.,
    /// Authorization) are added in [`execute()`](Self::execute).
    pub(crate) fn new(
        plan: HttpRequestPlan,
        verbosity: tempo_common::cli::Verbosity,
        network: Option<NetworkId>,
        dry_run: bool,
        max_spend: Option<String>,
    ) -> HttpResult<Self> {
        let verbose_connection = verbosity.debug_enabled();
        let mut builder = reqwest::Client::builder().connection_verbose(verbose_connection);

        if let Some(timeout) = plan.timeout_secs {
            builder = builder.timeout(Duration::from_secs(timeout));
        }

        if let Some(connect_timeout) = plan.connect_timeout_secs {
            builder = builder.connect_timeout(Duration::from_secs(connect_timeout));
        }

        if plan.follow_redirects {
            let limit = plan.follow_redirects_limit.unwrap_or(10);
            builder = builder.redirect(reqwest::redirect::Policy::limited(limit));
        } else {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }

        builder = builder.user_agent(&plan.user_agent);

        if plan.insecure {
            builder = builder.danger_accept_invalid_certs(true);
        }

        if plan.no_proxy {
            builder = builder.no_proxy();
        } else if let Some(ref p) = plan.proxy {
            let proxy =
                reqwest::Proxy::all(p).map_err(|source| ConfigError::InvalidProxyUrl { source })?;
            builder = builder.proxy(proxy);
        }

        if plan.http1_only {
            builder = builder.http1_only();
        } else if plan.http2 {
            builder = builder.http2_adaptive_window(true);
        }

        if !plan.headers.is_empty() {
            let mut header_map = reqwest::header::HeaderMap::new();
            for (name, value) in &plan.headers {
                let header_name = match reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
                    Ok(n) => n,
                    Err(e) => {
                        warn!(header_name = %name, error = %e, "dropping header with invalid name");
                        continue;
                    }
                };
                let header_value = match reqwest::header::HeaderValue::from_str(value) {
                    Ok(v) => v,
                    Err(e) => {
                        let safe = tempo_common::security::redact_header_value(name, value);
                        warn!(header_name = %name, header_value = %safe, error = %e, "dropping header with invalid value");
                        continue;
                    }
                };
                header_map.insert(header_name, header_value);
            }
            builder = builder.default_headers(header_map);
        }

        let client = builder.build().map_err(NetworkError::Reqwest)?;

        Ok(Self {
            plan,
            client,
            verbosity,
            network,
            dry_run,
            max_spend,
        })
    }

    /// The underlying reqwest client.
    ///
    /// Used by session flows that need direct access to the reqwest client.
    pub(crate) const fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// The HTTP method from the request plan.
    pub(crate) const fn method(&self) -> &reqwest::Method {
        &self.plan.method
    }

    /// The request body from the request plan, if any.
    pub(crate) fn body(&self) -> Option<&HttpRequestBody> {
        self.plan.body.as_ref()
    }

    /// Build a reqwest `Form` from stored multipart field specs.
    fn build_multipart_form(fields: &[MultipartField]) -> reqwest::multipart::Form {
        let mut form = reqwest::multipart::Form::new();
        for field in fields {
            match field {
                MultipartField::Text { name, value } => {
                    form = form.text(name.clone(), value.clone());
                }
                MultipartField::File {
                    name,
                    filename,
                    content_type,
                    bytes,
                } => {
                    let part =
                        reqwest::multipart::Part::bytes(bytes.clone()).file_name(filename.clone());
                    let part = match content_type {
                        Some(mime) => part.mime_str(mime).unwrap_or_else(|_| {
                            reqwest::multipart::Part::bytes(bytes.clone())
                                .file_name(filename.clone())
                        }),
                        None => part,
                    };
                    form = form.part(name.clone(), part);
                }
            }
        }
        form
    }

    /// Apply the request body to a request builder (standalone variant for callers
    /// that hold a body reference rather than the full plan).
    pub(crate) fn apply_body_from(
        req: reqwest::RequestBuilder,
        body: Option<&HttpRequestBody>,
    ) -> reqwest::RequestBuilder {
        match body {
            Some(HttpRequestBody::Bytes(data)) => req.body(data.clone()),
            Some(HttpRequestBody::Multipart(fields)) => {
                req.multipart(Self::build_multipart_form(fields))
            }
            None => req,
        }
    }

    /// Apply the request body to a request builder.
    fn apply_body(
        req: reqwest::RequestBuilder,
        body: &Option<HttpRequestBody>,
    ) -> reqwest::RequestBuilder {
        match body {
            Some(HttpRequestBody::Bytes(data)) => req.body(data.clone()),
            Some(HttpRequestBody::Multipart(fields)) => {
                req.multipart(Self::build_multipart_form(fields))
            }
            None => req,
        }
    }

    /// Build a raw reqwest request from the plan for streaming use.
    ///
    /// This encapsulates plan field access so callers don't need to
    /// reach into `plan.method`, `plan.headers`, and `plan.body` directly.
    pub(crate) fn build_raw_request(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.client.request(self.plan.method.clone(), url);
        for (name, value) in &self.plan.headers {
            req = req.header(name.as_str(), value.as_str());
        }
        req = Self::apply_body(req, &self.plan.body);
        req
    }

    /// Whether agent-level log messages should be printed (`-v`).
    pub(crate) const fn log_enabled(&self) -> bool {
        self.verbosity.log_enabled()
    }

    /// Whether debug-level log messages should be printed (`-vv`).
    pub(crate) const fn debug_enabled(&self) -> bool {
        self.verbosity.debug_enabled()
    }

    /// Execute an HTTP request and return the raw `reqwest::Response` with
    /// the same retry behavior as [`execute()`](Self::execute).
    ///
    /// This keeps transport resiliency (connect/timeout + configured status
    /// retries) while letting callers stream the response body directly.
    pub(crate) async fn execute_raw_with_retry(
        &self,
        url: &str,
        extra_headers: &[(String, String)],
    ) -> HttpResult<reqwest::Response> {
        self.execute_raw_with_retry_inner(self.plan.method.clone(), url, extra_headers, true)
            .await
    }

    async fn execute_raw_with_retry_inner(
        &self,
        method: reqwest::Method,
        url: &str,
        extra_headers: &[(String, String)],
        include_body: bool,
    ) -> HttpResult<reqwest::Response> {
        let plan = &self.plan;
        let mut attempt: u32 = 0;
        let mut backoff = plan.base_backoff_ms;

        loop {
            let result: HttpResult<reqwest::Response> = async {
                let mut req = self.client.request(method.clone(), url);
                for (name, value) in extra_headers {
                    req = req.header(name.as_str(), value.as_str());
                }
                if include_body {
                    req = Self::apply_body(req, &plan.body);
                }
                let response = req.send().await.map_err(NetworkError::Reqwest)?;
                Ok(response)
            }
            .await;

            match result {
                Ok(resp) => {
                    let status_code = resp.status().as_u16();
                    // HTTP status-based retry
                    if attempt < plan.max_retries
                        && !plan.retry_status_codes.is_empty()
                        && plan.retry_status_codes.contains(&status_code)
                    {
                        // Compute delay: Retry-After header or exponential backoff
                        let mut delay_ms = if plan.honor_retry_after {
                            if let Some(ra) = resp.headers().get("retry-after") {
                                ra.to_str()
                                    .ok()
                                    .and_then(|v| v.trim().parse::<u64>().ok())
                                    .map(|s| s.saturating_mul(1000))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                        .unwrap_or(backoff);

                        // Apply jitter if configured
                        if let Some(pct) = plan.retry_jitter_pct {
                            let jitter = ((delay_ms as f64) * (f64::from(pct) / 100.0)) as u64;
                            if jitter > 0 {
                                // Best-effort random jitter to avoid synchronized retries.
                                let mut bytes = [0u8; 8];
                                let rand = if getrandom::fill(&mut bytes).is_ok() {
                                    u64::from_le_bytes(bytes) % jitter
                                } else {
                                    // Deterministic fallback when entropy is unavailable.
                                    u64::from(attempt).saturating_mul(997) % jitter
                                };
                                delay_ms = delay_ms.saturating_add(rand);
                            }
                        }

                        if self.debug_enabled() {
                            eprintln!(
                                "[retry {} of {} on HTTP {} after {}ms]",
                                attempt + 1,
                                plan.max_retries,
                                status_code,
                                delay_ms
                            );
                        }
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        attempt += 1;
                        backoff = (backoff.saturating_mul(2)).min(plan.max_backoff_ms);
                        continue;
                    }

                    return Ok(resp);
                }
                Err(e) => {
                    let is_transient = matches!(
                        &e,
                        TempoError::Network(NetworkError::Reqwest(re))
                            if re.is_connect() || re.is_timeout()
                    );
                    if is_transient && attempt < plan.max_retries {
                        let delay_ms = backoff;
                        if self.debug_enabled() {
                            eprintln!(
                                "[retry {} of {} after {}ms: {}]",
                                attempt + 1,
                                plan.max_retries,
                                delay_ms,
                                e
                            );
                        }
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        attempt += 1;
                        backoff = (backoff.saturating_mul(2)).min(plan.max_backoff_ms);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Execute an HTTP request with retry logic.
    ///
    /// Extra headers (e.g., Authorization) are added per-request, not baked
    /// into the client. This enables connection pooling: the same client can
    /// serve the initial 402 request and the payment replay, skipping the
    /// second TLS handshake.
    pub(crate) async fn execute(
        &self,
        url: &str,
        extra_headers: &[(String, String)],
    ) -> HttpResult<HttpResponse> {
        let response = self.execute_raw_with_retry(url, extra_headers).await?;
        HttpResponse::from_reqwest(response).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get};

    use super::*;

    fn test_client(plan: HttpRequestPlan) -> HttpClient {
        HttpClient::new(
            plan,
            tempo_common::cli::Verbosity {
                level: 0,
                show_output: false,
            },
            None,
            false,
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_status_retry_honored_and_succeeds() {
        #[derive(Clone)]
        struct Ctx(Arc<Mutex<u32>>);

        let ctx = Ctx(Arc::new(Mutex::new(0)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        let app = axum::Router::new()
            .route(
                "/",
                get(|State(ctx): State<Ctx>| async move {
                    let mut n = ctx.0.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        ([("retry-after", "0")], StatusCode::SERVICE_UNAVAILABLE).into_response()
                    } else {
                        (StatusCode::OK, "ok").into_response()
                    }
                }),
            )
            .with_state(ctx.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = test_client(HttpRequestPlan {
            timeout_secs: Some(2),
            connect_timeout_secs: Some(1),
            no_proxy: true,
            max_retries: 2,
            base_backoff_ms: 1,
            max_backoff_ms: 10,
            retry_status_codes: vec![503],
            honor_retry_after: true,
            ..Default::default()
        });
        let resp = client
            .execute(&url, &[])
            .await
            .expect("should succeed after retry");
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.body_string().unwrap(), "ok");
        // Ensure it retried at least once
        assert!(*ctx.0.lock().unwrap() >= 2);
    }

    #[tokio::test]
    async fn test_raw_status_retry_honored_and_succeeds() {
        #[derive(Clone)]
        struct Ctx(Arc<Mutex<u32>>);

        let ctx = Ctx(Arc::new(Mutex::new(0)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        let app = axum::Router::new()
            .route(
                "/",
                get(|State(ctx): State<Ctx>| async move {
                    let mut n = ctx.0.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        ([("retry-after", "0")], StatusCode::SERVICE_UNAVAILABLE).into_response()
                    } else {
                        (StatusCode::OK, "ok").into_response()
                    }
                }),
            )
            .with_state(ctx.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = test_client(HttpRequestPlan {
            timeout_secs: Some(2),
            connect_timeout_secs: Some(1),
            no_proxy: true,
            max_retries: 2,
            base_backoff_ms: 1,
            max_backoff_ms: 10,
            retry_status_codes: vec![503],
            honor_retry_after: true,
            ..Default::default()
        });
        let resp = client
            .execute_raw_with_retry(&url, &[])
            .await
            .expect("raw request should succeed after retry");
        assert_eq!(resp.status().as_u16(), 200);
        assert!(*ctx.0.lock().unwrap() >= 2);
    }

    #[tokio::test]
    async fn test_transient_connect_retry_eventually_succeeds() {
        #[derive(Clone)]
        struct Ctx(Arc<Mutex<u32>>);

        let ctx = Ctx(Arc::new(Mutex::new(0)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        // First request sleeps longer than the client timeout, triggering a
        // timeout error (is_timeout()). Subsequent requests return immediately.
        let ctx_clone = ctx.clone();
        let app = axum::Router::new()
            .route(
                "/",
                get(|State(ctx): State<Ctx>| async move {
                    let n = {
                        let mut n = ctx.0.lock().unwrap();
                        *n += 1;
                        *n
                    };
                    if n <= 1 {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    "ok"
                }),
            )
            .with_state(ctx_clone);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = test_client(HttpRequestPlan {
            timeout_secs: Some(1),
            connect_timeout_secs: Some(1),
            no_proxy: true,
            max_retries: 3,
            base_backoff_ms: 10,
            max_backoff_ms: 100,
            ..Default::default()
        });
        let resp = client
            .execute(&url, &[])
            .await
            .expect("should succeed after retrying past timeout");
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.body_string().unwrap(), "ok");
        assert!(
            *ctx.0.lock().unwrap() >= 2,
            "should have retried at least once"
        );
    }

    #[tokio::test]
    async fn test_raw_transient_connect_retry_eventually_succeeds() {
        #[derive(Clone)]
        struct Ctx(Arc<Mutex<u32>>);

        let ctx = Ctx(Arc::new(Mutex::new(0)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        // First request sleeps longer than the client timeout, triggering a
        // timeout error. Subsequent requests return immediately.
        let ctx_clone = ctx.clone();
        let app = axum::Router::new()
            .route(
                "/",
                get(|State(ctx): State<Ctx>| async move {
                    let n = {
                        let mut n = ctx.0.lock().unwrap();
                        *n += 1;
                        *n
                    };
                    if n <= 1 {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    "ok"
                }),
            )
            .with_state(ctx_clone);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = test_client(HttpRequestPlan {
            timeout_secs: Some(1),
            connect_timeout_secs: Some(1),
            no_proxy: true,
            max_retries: 3,
            base_backoff_ms: 10,
            max_backoff_ms: 100,
            ..Default::default()
        });
        let resp = client
            .execute_raw_with_retry(&url, &[])
            .await
            .expect("raw request should succeed after retrying past timeout");
        assert_eq!(resp.status().as_u16(), 200);
        assert!(
            *ctx.0.lock().unwrap() >= 2,
            "should have retried at least once"
        );
    }

    #[tokio::test]
    async fn test_retry_jitter_applies_bounded_delay() {
        #[derive(Clone)]
        struct Ctx(Arc<Mutex<Vec<std::time::Instant>>>);

        let ctx = Ctx(Arc::new(Mutex::new(Vec::new())));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        let ctx_clone = ctx.clone();
        let app = axum::Router::new()
            .route(
                "/",
                get(|State(ctx): State<Ctx>| async move {
                    let mut times = ctx.0.lock().unwrap();
                    times.push(std::time::Instant::now());
                    if times.len() < 3 {
                        (StatusCode::SERVICE_UNAVAILABLE, "retry").into_response()
                    } else {
                        (StatusCode::OK, "ok").into_response()
                    }
                }),
            )
            .with_state(ctx_clone);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = test_client(HttpRequestPlan {
            timeout_secs: Some(5),
            connect_timeout_secs: Some(2),
            no_proxy: true,
            max_retries: 5,
            base_backoff_ms: 100,
            max_backoff_ms: 1000,
            retry_status_codes: vec![503],
            retry_jitter_pct: Some(50),
            ..Default::default()
        });
        let resp = client
            .execute(&url, &[])
            .await
            .expect("should succeed after retries");
        assert_eq!(resp.status_code, 200);

        let times = ctx.0.lock().unwrap();
        assert_eq!(times.len(), 3, "should have 3 requests total");

        // With base_backoff_ms=100 and jitter_pct=50, first delay is 100..150ms.
        // Second delay: backoff doubles to 200ms, jitter adds 0..100ms → 200..300ms.
        let gap1 = times[1].duration_since(times[0]);
        let gap2 = times[2].duration_since(times[1]);

        assert!(
            gap1.as_millis() >= 90,
            "first gap should be >= 90ms, was {}ms",
            gap1.as_millis()
        );
        assert!(
            gap1.as_millis() <= 300,
            "first gap should be <= 300ms, was {}ms",
            gap1.as_millis()
        );

        assert!(
            gap2.as_millis() >= 150,
            "second gap should be >= 150ms, was {}ms",
            gap2.as_millis()
        );
        assert!(
            gap2.as_millis() <= 500,
            "second gap should be <= 500ms, was {}ms",
            gap2.as_millis()
        );
    }

    #[tokio::test]
    async fn test_execute_extra_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        let app = axum::Router::new().route(
            "/",
            get(|headers: axum::http::HeaderMap| async move {
                if headers.contains_key("authorization") {
                    (StatusCode::OK, "authorized").into_response()
                } else {
                    (StatusCode::OK, "no-auth").into_response()
                }
            }),
        );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = test_client(HttpRequestPlan {
            timeout_secs: Some(2),
            connect_timeout_secs: Some(1),
            no_proxy: true,
            ..Default::default()
        });

        // Without extra headers
        let resp = client.execute(&url, &[]).await.unwrap();
        assert_eq!(resp.body_string().unwrap(), "no-auth");

        // Same client, with extra headers — verifies per-request headers
        // work without baking them into the client.
        let headers = vec![("Authorization".to_string(), "Bearer tok".to_string())];
        let resp = client.execute(&url, &headers).await.unwrap();
        assert_eq!(resp.body_string().unwrap(), "authorized");
    }
}
