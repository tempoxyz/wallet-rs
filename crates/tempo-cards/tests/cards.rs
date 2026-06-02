//! Integration tests for tempo wallet cards commands.

mod common;

use std::sync::{Arc, Mutex};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
    Router,
};
use common::test_command;
use serde_json::json;
use tempo_test::TestConfigBuilder;

const MAINNET_KEYS_TOML: &str = r#"
[[keys]]
wallet_address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
key_address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
chain_id = 4217
"#;

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    query: Option<String>,
    body: String,
    idempotency_key: Option<String>,
    authorization: Option<String>,
}

#[derive(Clone)]
struct CardsMock {
    bridge_url: String,
    stripe_url: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl CardsMock {
    async fn start() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/", any(handle_cards_mock))
            .route("/{*path}", any(handle_cards_mock))
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base = format!("http://127.0.0.1:{port}");
        Self {
            bridge_url: format!("{base}/v0"),
            stripe_url: format!("{base}/v1"),
            requests,
        }
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

async fn handle_cards_mock(
    State(requests): State<Arc<Mutex<Vec<CapturedRequest>>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let body_text = String::from_utf8_lossy(&body).to_string();
    let path = uri.path().to_string();
    let query = uri.query().map(ToString::to_string);
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    requests.lock().unwrap().push(CapturedRequest {
        method: method.to_string(),
        path: path.clone(),
        query: query.clone(),
        body: body_text,
        idempotency_key,
        authorization,
    });

    match (method.as_str(), path.as_str()) {
        ("POST", "/") => (
            StatusCode::OK,
            axum::Json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": format!("0x{:064x}", 1_234_567u64),
            })),
        )
            .into_response(),
        ("POST", "/v0/customers") => (
            StatusCode::OK,
            axum::Json(json!({
                "id": "cust_123",
                "type": "individual",
                "email": "john@example.com",
                "stripe_cardholder_id": null,
            })),
        )
            .into_response(),
        ("GET", "/v0/customers") => (
            StatusCode::OK,
            axum::Json(json!({
                "data": [{"id": "cust_123"}],
                "has_more": false,
            })),
        )
            .into_response(),
        ("GET", "/v0/customers/cust_123") => (
            StatusCode::OK,
            axum::Json(json!({
                "id": "cust_123",
                "email": "john@example.com",
            })),
        )
            .into_response(),
        ("DELETE", "/v0/customers/cust_123") => (
            StatusCode::OK,
            axum::Json(json!({
                "id": "cust_123",
                "deleted": true,
            })),
        )
            .into_response(),
        ("POST", "/v0/customers/tos_links") => (
            StatusCode::OK,
            axum::Json(json!({
                "url": "https://bridge.example/tos",
            })),
        )
            .into_response(),
        ("GET", "/v0/customers/cust_123/tos_acceptance_link") => (
            StatusCode::OK,
            axum::Json(json!({
                "url": "https://bridge.example/tos/cust_123",
            })),
        )
            .into_response(),
        ("GET", "/v0/customers/cust_123/kyc_link") => (
            StatusCode::OK,
            axum::Json(json!({
                "url": "https://bridge.example/kyc/cust_123",
                "endorsement": query.unwrap_or_default(),
            })),
        )
            .into_response(),
        ("GET", "/v0/customers/cust_123/transfers") => (
            StatusCode::OK,
            axum::Json(json!({
                "data": [{"id": "transfer_123"}],
                "has_more": false,
            })),
        )
            .into_response(),
        ("POST", "/v1/issuing/cards") => (
            StatusCode::OK,
            [("request-id", "req_card_create")],
            axum::Json(json!({
                "id": "ic_123",
                "object": "issuing.card",
                "type": "virtual",
                "status": "active",
            })),
        )
            .into_response(),
        ("GET", "/v1/issuing/cards") => (
            StatusCode::OK,
            axum::Json(json!({
                "object": "list",
                "data": [{"id": "ic_123"}],
                "has_more": false,
            })),
        )
            .into_response(),
        ("GET", "/v1/issuing/cards/ic_123") => (
            StatusCode::OK,
            axum::Json(json!({
                "id": "ic_123",
                "object": "issuing.card",
                "status": "active",
            })),
        )
            .into_response(),
        ("POST", "/v1/issuing/cards/ic_123") => (
            StatusCode::OK,
            axum::Json(json!({
                "id": "ic_123",
                "object": "issuing.card",
                "status": "updated",
            })),
        )
            .into_response(),
        ("GET", "/v1/issuing/cardholders") => (
            StatusCode::OK,
            axum::Json(json!({
                "object": "list",
                "data": [{"id": "ich_123"}],
                "has_more": false,
            })),
        )
            .into_response(),
        ("GET", "/v1/issuing/cardholders/ich_123") => (
            StatusCode::OK,
            axum::Json(json!({
                "id": "ich_123",
                "object": "issuing.cardholder",
            })),
        )
            .into_response(),
        ("GET", "/v1/issuing/transactions") => (
            StatusCode::OK,
            axum::Json(json!({
                "object": "list",
                "data": [{"id": "ipi_123"}],
                "has_more": false,
            })),
        )
            .into_response(),
        ("GET", "/v1/issuing/transactions/ipi_123") => (
            StatusCode::OK,
            axum::Json(json!({
                "id": "ipi_123",
                "object": "issuing.transaction",
            })),
        )
            .into_response(),
        ("GET", "/v1/issuing/authorizations") => (
            StatusCode::OK,
            axum::Json(json!({
                "object": "list",
                "data": [{"id": "iauth_123"}],
                "has_more": false,
            })),
        )
            .into_response(),
        ("GET", "/v1/issuing/authorizations/iauth_123") => (
            StatusCode::OK,
            axum::Json(json!({
                "id": "iauth_123",
                "object": "issuing.authorization",
            })),
        )
            .into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "error": format!("unhandled {method} {path}"),
            })),
        )
            .into_response(),
    }
}

fn parsed_stdout(output: &std::process::Output) -> serde_json::Value {
    assert!(output.status.success(), "command failed: {output:?}");
    serde_json::from_slice(&output.stdout).expect("stdout should be json")
}

fn card_command(temp: &tempfile::TempDir) -> std::process::Command {
    let mut cmd = test_command(temp);
    cmd.env_remove("TEMPO_BRIDGE_API_KEY");
    cmd.env_remove("BRIDGE_API_KEY");
    cmd.env_remove("TEMPO_STRIPE_API_KEY");
    cmd.env_remove("STRIPE_SECRET_KEY");
    cmd.env_remove("STRIPE_API_KEY");
    cmd.env_remove("TEMPO_BRIDGE_API_URL");
    cmd.env_remove("TEMPO_STRIPE_API_URL");
    cmd
}

fn query_pairs(request: &CapturedRequest) -> std::collections::HashMap<String, String> {
    url::form_urlencoded::parse(request.query.as_deref().unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

fn form_pairs(request: &CapturedRequest) -> std::collections::HashMap<String, String> {
    url::form_urlencoded::parse(request.body.as_bytes())
        .into_owned()
        .collect()
}

#[test]
fn cards_help_covers_all_new_leaf_commands() {
    let temp = TestConfigBuilder::new().build();
    let leaf_commands: &[&[&str]] = &[
        &["config", "bridge-api-key", "--help"],
        &["config", "stripe-api-key", "--help"],
        &["config", "show", "--help"],
        &["customers", "create", "--help"],
        &["customers", "get", "--help"],
        &["customers", "list", "--help"],
        &["customers", "delete", "--help"],
        &["customers", "tos-link", "--help"],
        &["customers", "tos-acceptance-link", "--help"],
        &["customers", "kyc-link", "--help"],
        &["customers", "transfers", "--help"],
        &["create", "--help"],
        &["list", "--help"],
        &["get", "--help"],
        &["update", "--help"],
        &["freeze", "--help"],
        &["unfreeze", "--help"],
        &["cancel", "--help"],
        &["cardholders", "list", "--help"],
        &["cardholders", "get", "--help"],
        &["transactions", "list", "--help"],
        &["transactions", "get", "--help"],
        &["authorizations", "list", "--help"],
        &["authorizations", "get", "--help"],
        &["approve", "--help"],
        &["allowance", "--help"],
    ];

    for args in leaf_commands {
        let output = card_command(&temp).args(*args).output().unwrap();
        assert!(
            output.status.success(),
            "help failed for {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn cards_config_save_and_show_masks_keys() {
    let temp = TestConfigBuilder::new().build();

    let bridge = card_command(&temp)
        .args(["-j", "config", "bridge-api-key", "sk-test-abcdef123456"])
        .output()
        .unwrap();
    let bridge_json = parsed_stdout(&bridge);
    assert_eq!(bridge_json["saved"], true);
    assert_eq!(bridge_json["environment"], "sandbox");

    let stripe = card_command(&temp)
        .args(["-j", "config", "stripe-api-key", "sk_test_abcdef123456"])
        .output()
        .unwrap();
    let stripe_json = parsed_stdout(&stripe);
    assert_eq!(stripe_json["saved"], true);
    assert_eq!(stripe_json["mode"], "test");

    let show = card_command(&temp)
        .args(["-j", "config", "show"])
        .output()
        .unwrap();
    let show_json = parsed_stdout(&show);
    assert_eq!(show_json["bridge"]["api_key"], "sk-test-abcd...3456");
    assert_eq!(show_json["stripe"]["api_key"], "sk_test_abcd...3456");
    assert!(show_json["config"]
        .as_str()
        .unwrap()
        .ends_with("cards.toml"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cards_customers_create_and_kyc_link_call_bridge() {
    let mock = CardsMock::start().await;
    let temp = TestConfigBuilder::new().build();

    let create = card_command(&temp)
        .env("TEMPO_BRIDGE_API_URL", &mock.bridge_url)
        .env("BRIDGE_API_KEY", "sk-test-mock")
        .args([
            "-j",
            "customers",
            "create",
            "--first-name",
            "John",
            "--last-name",
            "Doe",
            "--email",
            "john@example.com",
        ])
        .output()
        .unwrap();
    let create_json = parsed_stdout(&create);
    assert_eq!(create_json["id"], "cust_123");

    let kyc = card_command(&temp)
        .env("TEMPO_BRIDGE_API_URL", &mock.bridge_url)
        .env("BRIDGE_API_KEY", "sk-test-mock")
        .args([
            "-j",
            "customers",
            "kyc-link",
            "cust_123",
            "--endorsement",
            "cards",
        ])
        .output()
        .unwrap();
    let kyc_json = parsed_stdout(&kyc);
    assert_eq!(kyc_json["url"], "https://bridge.example/kyc/cust_123");

    let requests = mock.requests();
    let create_request = requests
        .iter()
        .find(|request| request.method == "POST" && request.path == "/v0/customers")
        .expect("captured customer create");
    let body: serde_json::Value = serde_json::from_str(&create_request.body).unwrap();
    assert_eq!(body["first_name"], "John");
    assert_eq!(body["last_name"], "Doe");

    let kyc_request = requests
        .iter()
        .find(|request| request.path == "/v0/customers/cust_123/kyc_link")
        .expect("captured kyc link");
    assert_eq!(kyc_request.query.as_deref(), Some("endorsement=cards"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cards_customer_management_commands_call_bridge() {
    let mock = CardsMock::start().await;
    let temp = TestConfigBuilder::new().build();

    let cases: &[(&[&str], &str, &str)] = &[
        (&["customers", "list"], "GET", "/v0/customers"),
        (
            &["customers", "get", "cust_123"],
            "GET",
            "/v0/customers/cust_123",
        ),
        (
            &["customers", "delete", "cust_123"],
            "DELETE",
            "/v0/customers/cust_123",
        ),
        (
            &["customers", "tos-link"],
            "POST",
            "/v0/customers/tos_links",
        ),
        (
            &["customers", "tos-acceptance-link", "cust_123"],
            "GET",
            "/v0/customers/cust_123/tos_acceptance_link",
        ),
        (
            &["customers", "transfers", "cust_123"],
            "GET",
            "/v0/customers/cust_123/transfers",
        ),
    ];

    for (args, _, _) in cases {
        let output = card_command(&temp)
            .env("TEMPO_BRIDGE_API_URL", &mock.bridge_url)
            .env("BRIDGE_API_KEY", "sk-test-mock")
            .arg("-j")
            .args(*args)
            .output()
            .unwrap();
        parsed_stdout(&output);
    }

    let requests = mock.requests();
    for (_, method, path) in cases {
        assert!(
            requests
                .iter()
                .any(|request| request.method == *method && request.path == *path),
            "missing captured request {method} {path}: {requests:#?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cards_create_defaults_to_wallet_and_posts_stripe_form() {
    let mock = CardsMock::start().await;
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MAINNET_KEYS_TOML)
        .build();

    let output = card_command(&temp)
        .env("TEMPO_STRIPE_API_URL", &mock.stripe_url)
        .env("STRIPE_SECRET_KEY", "sk_test_mock")
        .args([
            "-j",
            "create",
            "--cardholder",
            "ich_123",
            "--bridge-customer-id",
            "cust_123",
        ])
        .output()
        .unwrap();
    let parsed = parsed_stdout(&output);
    assert_eq!(parsed["id"], "ic_123");
    assert_eq!(parsed["stripe_request_id"], "req_card_create");

    let requests = mock.requests();
    let create_request = requests
        .iter()
        .find(|request| request.method == "POST" && request.path == "/v1/issuing/cards")
        .expect("captured card create");
    assert!(create_request
        .authorization
        .as_deref()
        .is_some_and(|value| value.starts_with("Basic ")));
    assert_eq!(
        create_request.idempotency_key.as_deref(),
        Some("tempo-cards-0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266-ich_123")
    );

    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(create_request.body.as_bytes())
            .into_owned()
            .collect();
    assert_eq!(form.get("cardholder").map(String::as_str), Some("ich_123"));
    assert_eq!(form.get("currency").map(String::as_str), Some("usd"));
    assert_eq!(form.get("type").map(String::as_str), Some("virtual"));
    assert_eq!(
        form.get("crypto_wallet[chain]").map(String::as_str),
        Some("tempo")
    );
    assert_eq!(
        form.get("crypto_wallet[currency]").map(String::as_str),
        Some("usdc")
    );
    assert_eq!(
        form.get("crypto_wallet[address]").map(String::as_str),
        Some("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266")
    );
    assert_eq!(
        form.get("metadata[bridge_customer_id]").map(String::as_str),
        Some("cust_123")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cards_stripe_card_commands_call_expected_endpoints() {
    let mock = CardsMock::start().await;
    let temp = TestConfigBuilder::new().build();

    let cases: &[(&[&str], &str, &str)] = &[
        (
            &[
                "list",
                "--cardholder",
                "ich_123",
                "--status",
                "active",
                "--type",
                "virtual",
                "--last4",
                "4242",
                "--limit",
                "3",
            ],
            "GET",
            "/v1/issuing/cards",
        ),
        (&["get", "ic_123"], "GET", "/v1/issuing/cards/ic_123"),
        (
            &["update", "ic_123", "--status", "inactive"],
            "POST",
            "/v1/issuing/cards/ic_123",
        ),
        (&["freeze", "ic_123"], "POST", "/v1/issuing/cards/ic_123"),
        (&["unfreeze", "ic_123"], "POST", "/v1/issuing/cards/ic_123"),
        (
            &["cancel", "ic_123", "--cancellation-reason", "lost"],
            "POST",
            "/v1/issuing/cards/ic_123",
        ),
    ];

    for (args, _, _) in cases {
        let output = card_command(&temp)
            .env("TEMPO_STRIPE_API_URL", &mock.stripe_url)
            .env("STRIPE_SECRET_KEY", "sk_test_mock")
            .arg("-j")
            .args(*args)
            .output()
            .unwrap();
        parsed_stdout(&output);
    }

    let requests = mock.requests();
    for (_, method, path) in cases {
        assert!(
            requests
                .iter()
                .any(|request| request.method == *method && request.path == *path),
            "missing captured request {method} {path}: {requests:#?}"
        );
    }

    let list_request = requests
        .iter()
        .find(|request| request.method == "GET" && request.path == "/v1/issuing/cards")
        .expect("captured card list");
    let query = query_pairs(list_request);
    assert_eq!(query.get("cardholder").map(String::as_str), Some("ich_123"));
    assert_eq!(query.get("status").map(String::as_str), Some("active"));
    assert_eq!(query.get("type").map(String::as_str), Some("virtual"));
    assert_eq!(query.get("last4").map(String::as_str), Some("4242"));
    assert_eq!(query.get("limit").map(String::as_str), Some("3"));

    let updates: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "POST" && request.path == "/v1/issuing/cards/ic_123")
        .collect();
    assert!(updates
        .iter()
        .any(|request| form_pairs(request).get("status").map(String::as_str) == Some("inactive")));
    assert!(updates
        .iter()
        .any(|request| form_pairs(request).get("status").map(String::as_str) == Some("active")));
    assert!(updates.iter().any(|request| {
        let form = form_pairs(request);
        form.get("status").map(String::as_str) == Some("canceled")
            && form.get("cancellation_reason").map(String::as_str) == Some("lost")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cards_stripe_resource_commands_call_expected_endpoints() {
    let mock = CardsMock::start().await;
    let temp = TestConfigBuilder::new().build();

    let cases: &[(&[&str], &str)] = &[
        (
            &[
                "cardholders",
                "list",
                "--email",
                "john@example.com",
                "--status",
                "active",
                "--type",
                "individual",
                "--limit",
                "2",
            ],
            "/v1/issuing/cardholders",
        ),
        (
            &["cardholders", "get", "ich_123"],
            "/v1/issuing/cardholders/ich_123",
        ),
        (
            &[
                "transactions",
                "list",
                "--card",
                "ic_123",
                "--cardholder",
                "ich_123",
                "--type",
                "capture",
                "--limit",
                "2",
            ],
            "/v1/issuing/transactions",
        ),
        (
            &["transactions", "get", "ipi_123"],
            "/v1/issuing/transactions/ipi_123",
        ),
        (
            &[
                "authorizations",
                "list",
                "--card",
                "ic_123",
                "--cardholder",
                "ich_123",
                "--status",
                "pending",
                "--limit",
                "2",
            ],
            "/v1/issuing/authorizations",
        ),
        (
            &["authorizations", "get", "iauth_123"],
            "/v1/issuing/authorizations/iauth_123",
        ),
    ];

    for (args, _) in cases {
        let output = card_command(&temp)
            .env("TEMPO_STRIPE_API_URL", &mock.stripe_url)
            .env("STRIPE_SECRET_KEY", "sk_test_mock")
            .arg("-j")
            .args(*args)
            .output()
            .unwrap();
        parsed_stdout(&output);
    }

    let requests = mock.requests();
    for (_, path) in cases {
        assert!(
            requests
                .iter()
                .any(|request| request.method == "GET" && request.path == *path),
            "missing captured request GET {path}: {requests:#?}"
        );
    }
}

#[test]
fn cards_approve_dry_run_uses_default_tempo_issuer() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MAINNET_KEYS_TOML)
        .build();

    let output = card_command(&temp)
        .args(["-j", "approve", "--amount", "1.00", "--dry-run"])
        .output()
        .unwrap();
    let parsed = parsed_stdout(&output);
    assert_eq!(parsed["status"], "dry_run");
    assert_eq!(
        parsed["spender"],
        "0x3e8f24b686aa8c036038f7d557b70e6ce0e7b56b"
    );
    assert_eq!(parsed["amount_atomic"], "1000000");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cards_allowance_queries_rpc_and_formats_amount() {
    let mock = CardsMock::start().await;
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MAINNET_KEYS_TOML)
        .build();

    let output = card_command(&temp)
        .env("TEMPO_RPC_URL", mock.stripe_url.trim_end_matches("/v1"))
        .args(["-j", "allowance"])
        .output()
        .unwrap();
    let parsed = parsed_stdout(&output);
    assert_eq!(parsed["allowance_atomic"], "1234567");
    assert_eq!(parsed["allowance"], "1.234567");
    assert_eq!(
        parsed["spender"],
        "0x3e8f24b686aa8c036038f7d557b70e6ce0e7b56b"
    );

    let requests = mock.requests();
    let rpc_request = requests
        .iter()
        .find(|request| request.method == "POST" && request.path == "/")
        .expect("captured allowance eth_call");
    let rpc: serde_json::Value = serde_json::from_str(&rpc_request.body).unwrap();
    assert_eq!(rpc["method"], "eth_call");
}
