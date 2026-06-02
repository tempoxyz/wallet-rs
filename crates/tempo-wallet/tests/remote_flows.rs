mod common;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use axum::{routing::post, Json, Router};
use serde_json::json;
use tempo_test::{mock_rpc_response, TestConfigBuilder, MODERATO_DIRECT_KEYS_TOML};

use common::test_command;

const AUTHORIZED_WALLET_ADDRESS: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
const MODERATO_TOKEN_ADDRESS: &str = "0x20c0000000000000000000000000000000000000";
const BALANCE_OF_SELECTOR: &str = "70a08231";

struct MockLoginServer {
    base_url: String,
    poll_count: Arc<Mutex<u32>>,
    device_requests: Arc<Mutex<Vec<serde_json::Value>>>,
    poll_requests: Arc<Mutex<Vec<serde_json::Value>>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockLoginServer {
    async fn start_authorized(code: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}:{}", addr.ip(), addr.port());
        let poll_count = Arc::new(Mutex::new(0u32));
        let device_requests = Arc::new(Mutex::new(Vec::new()));
        let poll_requests = Arc::new(Mutex::new(Vec::new()));

        let device_code = code.to_string();
        let device_request_log = device_requests.clone();
        let poll_code = code.to_string();
        let poll_state = poll_count.clone();
        let poll_request_log = poll_requests.clone();

        let app = Router::new()
            .route(
                "/cli-auth/device-code",
                post(move |Json(body): Json<serde_json::Value>| {
                    let code = device_code.clone();
                    let device_request_log = device_request_log.clone();
                    async move {
                        device_request_log.lock().unwrap().push(body);
                        Json(json!({ "code": code }))
                    }
                }),
            )
            .route(
                "/cli-auth/poll/{code}",
                post(
                    move |axum::extract::Path(path_code): axum::extract::Path<String>,
                          Json(body): Json<serde_json::Value>| {
                        let expected = poll_code.clone();
                        let poll_state = poll_state.clone();
                        let poll_request_log = poll_request_log.clone();
                        async move {
                            assert_eq!(path_code, expected, "unexpected poll code");
                            poll_request_log.lock().unwrap().push(body);
                            let mut count = poll_state.lock().unwrap();
                            let response = if *count == 0 {
                                *count += 1;
                                json!({ "status": "pending" })
                            } else {
                                *count += 1;
                                json!({
                                    "status": "authorized",
                                    "account_address": AUTHORIZED_WALLET_ADDRESS,
                                    "key_authorization": null
                                })
                            };
                            Json(response)
                        }
                    },
                ),
            );

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        Self {
            base_url,
            poll_count,
            device_requests,
            poll_requests,
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
        }
    }

    fn auth_url(&self) -> String {
        format!("{}/cli-auth", self.base_url)
    }
}

impl Drop for MockLoginServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

struct BalanceSequenceRpcServer {
    base_url: String,
    balances: Arc<Mutex<VecDeque<String>>>,
    last_value: Arc<Mutex<String>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl BalanceSequenceRpcServer {
    async fn start(raw_balances: Vec<&str>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}:{}", addr.ip(), addr.port());

        let balances = Arc::new(Mutex::new(
            raw_balances
                .into_iter()
                .map(std::string::ToString::to_string)
                .collect(),
        ));
        let last_value = Arc::new(Mutex::new(String::from("0")));

        let shared_balances = balances.clone();
        let shared_last_value = last_value.clone();

        let app = Router::new().route(
            "/",
            post(move |Json(body): Json<serde_json::Value>| {
                let shared_balances = shared_balances.clone();
                let shared_last_value = shared_last_value.clone();
                async move {
                    if let Some(batch) = body.as_array() {
                        let response = serde_json::Value::Array(
                            batch
                                .iter()
                                .map(|req| {
                                    handle_rpc_request(req, &shared_balances, &shared_last_value)
                                })
                                .collect(),
                        );
                        Json(response)
                    } else {
                        Json(handle_rpc_request(
                            &body,
                            &shared_balances,
                            &shared_last_value,
                        ))
                    }
                }
            }),
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        Self {
            base_url,
            balances,
            last_value,
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
        }
    }
}

impl Drop for BalanceSequenceRpcServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn handle_rpc_request(
    req: &serde_json::Value,
    balances: &Arc<Mutex<VecDeque<String>>>,
    last_value: &Arc<Mutex<String>>,
) -> serde_json::Value {
    if is_fund_balance_request(req) {
        let raw = next_balance(balances, last_value);
        let encoded = encode_raw_balance(&raw);
        return json!({
            "jsonrpc": "2.0",
            "id": req["id"].clone(),
            "result": encoded,
        });
    }

    mock_rpc_response(req, 42431)
}

fn assert_remote_login_handoff(stderr: &str) {
    assert!(stderr.contains("Auth URL:"), "{stderr}");
    assert!(stderr.contains("network=testnet"), "{stderr}");
    assert!(stderr.contains("chain_id=42431"), "{stderr}");
    assert!(stderr.contains("Network: testnet"), "{stderr}");
    assert!(stderr.contains("Verification code:"), "{stderr}");
    assert!(stderr.contains("Open this link on your device"), "{stderr}");
    assert!(
        stderr.contains("If the wallet page shows that same code"),
        "{stderr}"
    );
    assert!(stderr.contains("tap Continue"), "{stderr}");
    assert!(
        stderr.contains("After passkey or wallet creation, return here"),
        "{stderr}"
    );
    assert!(stderr.contains("one more authorization link"), "{stderr}");
}

fn assert_login_server_saw_testnet(login: &MockLoginServer) {
    let device_requests = login.device_requests.lock().unwrap();
    assert_eq!(device_requests.len(), 1);
    assert_eq!(device_requests[0]["network"], "testnet");
    assert_eq!(device_requests[0]["chain_id"], 42431);

    let poll_requests = login.poll_requests.lock().unwrap();
    assert!(!poll_requests.is_empty());
    for req in poll_requests.iter() {
        assert_eq!(req["network"], "testnet");
        assert_eq!(req["chain_id"], 42431);
    }
}

fn assert_remote_fund_handoff(stderr: &str) {
    assert!(stderr.contains("Fund URL:"), "{stderr}");
    assert!(stderr.contains("Open this link on your device"), "{stderr}");
    assert!(stderr.contains("After funding is complete"), "{stderr}");
}

fn is_fund_balance_request(req: &serde_json::Value) -> bool {
    if req["method"].as_str() != Some("eth_call") {
        return false;
    }

    let Some(params) = req["params"].as_array() else {
        return false;
    };
    let Some(call) = params.first().and_then(serde_json::Value::as_object) else {
        return false;
    };
    let Some(to) = call.get("to").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some(data) = call
        .get("data")
        .or_else(|| call.get("input"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };

    normalized_hex(to) == normalized_hex(MODERATO_TOKEN_ADDRESS)
        && data.eq_ignore_ascii_case(&balance_of_call_data(AUTHORIZED_WALLET_ADDRESS))
}

fn next_balance(
    balances: &Arc<Mutex<VecDeque<String>>>,
    last_value: &Arc<Mutex<String>>,
) -> String {
    let next = {
        let mut queue = balances.lock().unwrap();
        queue.pop_front()
    };

    match next {
        Some(raw) => {
            *last_value.lock().unwrap() = raw.clone();
            raw
        }
        None => last_value.lock().unwrap().clone(),
    }
}

fn encode_raw_balance(raw: &str) -> String {
    let value = raw.parse::<u128>().unwrap();
    let bytes = alloy::primitives::U256::from(value).to_be_bytes::<32>();
    format!("0x{}", hex::encode(bytes))
}

fn normalized_hex(value: &str) -> String {
    value.trim_start_matches("0x").to_ascii_lowercase()
}

fn balance_of_call_data(account: &str) -> String {
    format!("0x{BALANCE_OF_SELECTOR}{:0>64}", normalized_hex(account))
}

fn moderato_config_toml(rpc_url: &str) -> String {
    format!("[rpc]\n\"tempo-moderato\" = \"{rpc_url}\"\n")
}

fn build_login_temp(rpc_url: &str) -> tempfile::TempDir {
    TestConfigBuilder::new()
        .with_config_toml(moderato_config_toml(rpc_url))
        .build()
}

fn build_fund_temp(rpc_url: &str) -> tempfile::TempDir {
    TestConfigBuilder::new()
        .with_config_toml(moderato_config_toml(rpc_url))
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build()
}

#[test]
fn unrelated_eth_call_uses_default_rpc_response_and_does_not_advance_balance_sequence() {
    let balances = Arc::new(Mutex::new(VecDeque::from([
        String::from("0"),
        String::from("1000000"),
    ])));
    let last_value = Arc::new(Mutex::new(String::from("0")));
    let request = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "eth_call",
        "params": [
            {
                "to": "0xe1c4d3dce17bc111181ddf716f75bae49e61a336",
                "data": "0x12345678"
            },
            "latest"
        ]
    });

    let response = handle_rpc_request(&request, &balances, &last_value);

    assert_eq!(response, mock_rpc_response(&request, 42431));
    assert_eq!(
        balances.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
        vec![String::from("0"), String::from("1000000")]
    );
    assert_eq!(last_value.lock().unwrap().as_str(), "0");
}

#[test]
fn matching_balance_request_advances_sequence_and_repeats_last_value() {
    let balances = Arc::new(Mutex::new(VecDeque::from([
        String::from("0"),
        String::from("1000000"),
    ])));
    let last_value = Arc::new(Mutex::new(String::from("0")));
    let request = json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "eth_call",
        "params": [
            {
                "to": MODERATO_TOKEN_ADDRESS,
                "input": balance_of_call_data(AUTHORIZED_WALLET_ADDRESS)
            },
            "latest"
        ]
    });

    let first = handle_rpc_request(&request, &balances, &last_value);
    let second = handle_rpc_request(&request, &balances, &last_value);
    let third = handle_rpc_request(&request, &balances, &last_value);

    assert_eq!(first["result"], encode_raw_balance("0"));
    assert_eq!(second["result"], encode_raw_balance("1000000"));
    assert_eq!(third["result"], encode_raw_balance("1000000"));
    assert!(balances.lock().unwrap().is_empty());
    assert_eq!(last_value.lock().unwrap().as_str(), "1000000");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_no_browser_prints_remote_safe_handoff_copy_and_completes() {
    let login = MockLoginServer::start_authorized("ANMGE375").await;
    let rpc = BalanceSequenceRpcServer::start(vec!["0"]).await;
    let temp = build_login_temp(&rpc.base_url);

    let output = test_command(&temp)
        .env("TEMPO_AUTH_URL", login.auth_url())
        .args(["-n", "tempo-moderato", "login", "--no-browser"])
        .output()
        .unwrap();

    assert!(output.status.success(), "login should succeed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_remote_login_handoff(&stderr);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Wallet"), "{stdout}");
    assert_eq!(*login.poll_count.lock().unwrap(), 2);
    assert_login_server_saw_testnet(&login);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_no_browser_json_keeps_structured_stdout_and_prints_remote_handoff() {
    let login = MockLoginServer::start_authorized("ANMGE375").await;
    let rpc = BalanceSequenceRpcServer::start(vec!["0"]).await;
    let temp = build_login_temp(&rpc.base_url);

    let output = test_command(&temp)
        .env("TEMPO_AUTH_URL", login.auth_url())
        .args(["-j", "-n", "tempo-moderato", "login", "--no-browser"])
        .output()
        .unwrap();

    assert!(output.status.success(), "login should succeed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_remote_login_handoff(&stderr);

    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["ready"], true, "{stdout}");
    assert_eq!(*login.poll_count.lock().unwrap(), 2);
    assert_login_server_saw_testnet(&login);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_no_browser_toon_keeps_structured_stdout_and_prints_remote_handoff() {
    let login = MockLoginServer::start_authorized("ANMGE375").await;
    let rpc = BalanceSequenceRpcServer::start(vec!["0"]).await;
    let temp = build_login_temp(&rpc.base_url);

    let output = test_command(&temp)
        .env("TEMPO_AUTH_URL", login.auth_url())
        .args(["-t", "-n", "tempo-moderato", "login", "--no-browser"])
        .output()
        .unwrap();

    assert!(output.status.success(), "login should succeed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_remote_login_handoff(&stderr);

    let stdout: serde_json::Value =
        toon_format::decode_default(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    assert_eq!(stdout["ready"], true, "{stdout}");
    assert_eq!(*login.poll_count.lock().unwrap(), 2);
    assert_login_server_saw_testnet(&login);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_default_flow_keeps_local_copy_and_does_not_print_remote_handoff_text() {
    let login = MockLoginServer::start_authorized("ANMGE375").await;
    let rpc = BalanceSequenceRpcServer::start(vec!["0"]).await;
    let temp = build_login_temp(&rpc.base_url);

    let output = test_command(&temp)
        .env("TEMPO_AUTH_URL", login.auth_url())
        .args(["-n", "tempo-moderato", "login"])
        .output()
        .unwrap();

    assert!(output.status.success(), "login should succeed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Auth URL:"), "{stderr}");
    assert!(stderr.contains("network=testnet"), "{stderr}");
    assert!(stderr.contains("chain_id=42431"), "{stderr}");
    assert!(stderr.contains("Network: testnet"), "{stderr}");
    assert!(stderr.contains("Verification code:"), "{stderr}");
    assert!(
        !stderr.contains("Open this link on your device"),
        "unexpected remote-safe handoff text: {stderr}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Wallet"), "{stdout}");
    assert_eq!(*login.poll_count.lock().unwrap(), 2);
    assert_login_server_saw_testnet(&login);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fund_no_browser_prints_remote_safe_handoff_copy_and_detects_balance_change() {
    let rpc = BalanceSequenceRpcServer::start(vec!["0", "1000000"]).await;
    let temp = build_fund_temp(&rpc.base_url);

    let output = test_command(&temp)
        .env("TEMPO_AUTH_URL", "https://wallet.tempo.xyz/cli-auth")
        .args(["-n", "tempo-moderato", "fund", "--no-browser"])
        .output()
        .unwrap();

    assert!(output.status.success(), "fund should succeed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_remote_fund_handoff(&stderr);
    assert!(rpc.balances.lock().unwrap().is_empty());
    assert_eq!(rpc.last_value.lock().unwrap().as_str(), "1000000");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fund_no_browser_json_prints_remote_handoff() {
    let rpc = BalanceSequenceRpcServer::start(vec!["0", "1000000"]).await;
    let temp = build_fund_temp(&rpc.base_url);

    let output = test_command(&temp)
        .env("TEMPO_AUTH_URL", "https://wallet.tempo.xyz/cli-auth")
        .args(["-j", "-n", "tempo-moderato", "fund", "--no-browser"])
        .output()
        .unwrap();

    assert!(output.status.success(), "fund should succeed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_remote_fund_handoff(&stderr);
    assert!(rpc.balances.lock().unwrap().is_empty());
    assert_eq!(rpc.last_value.lock().unwrap().as_str(), "1000000");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fund_no_browser_toon_prints_remote_handoff() {
    let rpc = BalanceSequenceRpcServer::start(vec!["0", "1000000"]).await;
    let temp = build_fund_temp(&rpc.base_url);

    let output = test_command(&temp)
        .env("TEMPO_AUTH_URL", "https://wallet.tempo.xyz/cli-auth")
        .args(["-t", "-n", "tempo-moderato", "fund", "--no-browser"])
        .output()
        .unwrap();

    assert!(output.status.success(), "fund should succeed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_remote_fund_handoff(&stderr);
    assert!(rpc.balances.lock().unwrap().is_empty());
    assert_eq!(rpc.last_value.lock().unwrap().as_str(), "1000000");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fund_default_flow_keeps_local_copy_and_does_not_print_remote_handoff_text() {
    let rpc = BalanceSequenceRpcServer::start(vec!["0", "1000000"]).await;
    let temp = build_fund_temp(&rpc.base_url);

    let output = test_command(&temp)
        .env("TEMPO_AUTH_URL", "https://wallet.tempo.xyz/cli-auth")
        .args(["-n", "tempo-moderato", "fund"])
        .output()
        .unwrap();

    assert!(output.status.success(), "fund should succeed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Fund URL:"), "{stderr}");
    assert!(
        !stderr.contains("Open this link on your device"),
        "unexpected remote-safe handoff text: {stderr}"
    );
    assert!(rpc.balances.lock().unwrap().is_empty());
    assert_eq!(rpc.last_value.lock().unwrap().as_str(), "1000000");
}
