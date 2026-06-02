//! Bridge and Stripe API clients for wallet-backed cards.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use reqwest::{header, Method, StatusCode, Url};
use serde_json::{Map, Value};

use tempo_common::{
    error::{ConfigError, NetworkError, TempoError},
    security::sanitize_error,
};

use super::config::{bridge_api_key, bridge_environment, stripe_api_key};

const BRIDGE_API_BASE: &str = "https://api.bridge.xyz/v0";
const BRIDGE_SANDBOX_API_BASE: &str = "https://api.sandbox.bridge.xyz/v0";
const STRIPE_API_BASE: &str = "https://api.stripe.com/v1";

#[derive(Clone)]
pub(super) struct BridgeClient {
    client: reqwest::Client,
    base_url: Url,
    api_key: String,
}

#[derive(Clone)]
pub(super) struct StripeClient {
    client: reqwest::Client,
    base_url: Url,
    api_key: String,
}

impl BridgeClient {
    pub(super) fn new() -> Result<Self, TempoError> {
        let secret = bridge_api_key()?.ok_or_else(|| {
            ConfigError::Missing(
                "No Bridge API key configured. Run `tempo wallet cards config bridge-api-key <key>` or set BRIDGE_API_KEY.".to_string(),
            )
        })?;
        let base_url = bridge_base_url(&secret.value)?;
        Ok(Self {
            client: http_client()?,
            base_url,
            api_key: secret.value,
        })
    }

    pub(super) async fn get(
        &self,
        path: &str,
        params: &[(&str, String)],
        operation: &'static str,
    ) -> Result<Value, TempoError> {
        let mut url = self.url(path)?;
        append_query(&mut url, params);
        self.request_json(Method::GET, url, None, operation).await
    }

    pub(super) async fn post(
        &self,
        path: &str,
        body: Option<Value>,
        operation: &'static str,
    ) -> Result<Value, TempoError> {
        let url = self.url(path)?;
        self.request_json(Method::POST, url, body, operation).await
    }

    pub(super) async fn delete(
        &self,
        path: &str,
        operation: &'static str,
    ) -> Result<Value, TempoError> {
        let url = self.url(path)?;
        self.request_json(Method::DELETE, url, None, operation)
            .await
    }

    async fn request_json(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        operation: &'static str,
    ) -> Result<Value, TempoError> {
        let mut req = self
            .client
            .request(method.clone(), url)
            .header("Api-Key", &self.api_key)
            .header(header::CONTENT_TYPE, "application/json");
        if matches!(method, Method::POST | Method::PUT) {
            req = req.header("Idempotency-Key", idempotency_key());
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.map_err(NetworkError::Reqwest)?;
        parse_json_response(resp, operation, None).await
    }

    fn url(&self, path: &str) -> Result<Url, TempoError> {
        self.base_url
            .join(path.trim_start_matches('/'))
            .map_err(|source| {
                ConfigError::InvalidUrl {
                    context: "Bridge API path",
                    source,
                }
                .into()
            })
    }
}

impl StripeClient {
    pub(super) fn new() -> Result<Self, TempoError> {
        let secret = stripe_api_key()?.ok_or_else(|| {
            ConfigError::Missing(
                "No Stripe API key configured. Run `tempo wallet cards config stripe-api-key <key>` or set STRIPE_SECRET_KEY.".to_string(),
            )
        })?;
        let base_url = stripe_base_url()?;
        Ok(Self {
            client: http_client()?,
            base_url,
            api_key: secret.value,
        })
    }

    pub(super) async fn get(
        &self,
        path: &str,
        params: &[(&str, String)],
        operation: &'static str,
    ) -> Result<Value, TempoError> {
        let mut url = self.url(path)?;
        append_query(&mut url, params);
        self.request_json(Method::GET, url, None, None, operation)
            .await
    }

    pub(super) async fn post_form(
        &self,
        path: &str,
        form: Vec<(&'static str, String)>,
        idempotency_key: Option<&str>,
        operation: &'static str,
    ) -> Result<Value, TempoError> {
        let url = self.url(path)?;
        self.request_json(Method::POST, url, Some(form), idempotency_key, operation)
            .await
    }

    async fn request_json(
        &self,
        method: Method,
        url: Url,
        form: Option<Vec<(&'static str, String)>>,
        idempotency_key: Option<&str>,
        operation: &'static str,
    ) -> Result<Value, TempoError> {
        let mut req = self
            .client
            .request(method, url)
            .headers(self.auth_headers()?);
        if let Some(key) = idempotency_key {
            req = req.header("Idempotency-Key", key);
        }
        if let Some(form) = form {
            req = req.form(&form);
        }
        let resp = req.send().await.map_err(NetworkError::Reqwest)?;
        let request_id = resp
            .headers()
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string);
        parse_json_response(resp, operation, request_id).await
    }

    fn auth_headers(&self) -> Result<header::HeaderMap, TempoError> {
        let auth = BASE64_STANDARD.encode(format!("{}:", self.api_key));
        let mut headers = header::HeaderMap::new();
        let value = header::HeaderValue::from_str(&format!("Basic {auth}")).map_err(|source| {
            ConfigError::Invalid(format!("invalid Stripe authorization header: {source}"))
        })?;
        headers.insert(header::AUTHORIZATION, value);
        Ok(headers)
    }

    fn url(&self, path: &str) -> Result<Url, TempoError> {
        self.base_url
            .join(path.trim_start_matches('/'))
            .map_err(|source| {
                ConfigError::InvalidUrl {
                    context: "Stripe API path",
                    source,
                }
                .into()
            })
    }
}

pub(super) fn path_segment(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

pub(super) fn idempotency_key() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

pub(super) fn maybe_push(
    params: &mut Vec<(&'static str, String)>,
    key: &'static str,
    value: Option<String>,
) {
    if let Some(value) = value {
        params.push((key, value));
    }
}

fn http_client() -> Result<reqwest::Client, TempoError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(format!("tempo-wallet/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(NetworkError::Reqwest)
        .map_err(TempoError::from)
}

fn bridge_base_url(api_key: &str) -> Result<Url, TempoError> {
    if let Ok(value) = std::env::var("TEMPO_BRIDGE_API_URL") {
        return parse_base_url(&value, "Bridge API base URL");
    }
    let url = match bridge_environment(api_key) {
        "sandbox" => BRIDGE_SANDBOX_API_BASE,
        _ => BRIDGE_API_BASE,
    };
    parse_base_url(url, "Bridge API base URL")
}

fn stripe_base_url() -> Result<Url, TempoError> {
    if let Ok(value) = std::env::var("TEMPO_STRIPE_API_URL") {
        return parse_base_url(&value, "Stripe API base URL");
    }
    parse_base_url(STRIPE_API_BASE, "Stripe API base URL")
}

fn parse_base_url(value: &str, context: &'static str) -> Result<Url, TempoError> {
    let mut parsed =
        Url::parse(value).map_err(|source| ConfigError::InvalidUrl { context, source })?;
    if !parsed.path().ends_with('/') {
        parsed.set_path(&format!("{}/", parsed.path().trim_end_matches('/')));
    }
    Ok(parsed)
}

fn append_query(url: &mut Url, params: &[(&str, String)]) {
    let mut query = url.query_pairs_mut();
    for (key, value) in params {
        query.append_pair(key, value);
    }
}

async fn parse_json_response(
    resp: reqwest::Response,
    operation: &'static str,
    request_id: Option<String>,
) -> Result<Value, TempoError> {
    let status = resp.status();
    if !status.is_success() {
        return Err(http_status_error(resp, operation).await);
    }

    let text = resp.text().await.map_err(NetworkError::Reqwest)?;
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let mut json =
        serde_json::from_str::<Value>(&text).map_err(|source| NetworkError::ResponseParse {
            context: operation,
            source,
        })?;
    if let (Some(request_id), Value::Object(ref mut map)) = (request_id, &mut json) {
        map.insert("stripe_request_id".to_string(), Value::String(request_id));
    }
    Ok(json)
}

async fn http_status_error(resp: reqwest::Response, operation: &'static str) -> TempoError {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .ok()
        .map(|text| sanitize_error(text.trim()))
        .filter(|text| !text.is_empty());
    NetworkError::HttpStatus {
        operation,
        status: status_code(status),
        body,
    }
    .into()
}

const fn status_code(status: StatusCode) -> u16 {
    status.as_u16()
}
