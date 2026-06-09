//! Session command integration scenarios and harnesses split from commands.rs.
// This module holds shared fixtures and mock servers used by close/list/sync split files.

use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use mpp::{Base64UrlJson, PaymentChallenge};
use serde_json::json;

use super::*;
use tempo_test::{corrupt_local_session_deposit, seed_local_session};

use tempo_common::network::TEMPO_MODERATO_ESCROW;
const MODERATO_TOKEN: &str = "0x20c0000000000000000000000000000000000000";
const CHANNEL_OPENED_TOPIC: &str =
    "0xcd6e60364f8ee4c2b0d62afc07a1fb04fd267ce94693f93f8f85daaa099b5c94";
const SEEDED_CHANNEL_ID: &str =
    "0x0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";
const SECOND_CHANNEL_ID: &str =
    "0x02030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f2021";
const ORPHANED_CHANNEL_ID: &str =
    "0x030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122";
const SEEDED_PAYER: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";

struct StoredCloseState {
    state: String,
    close_requested_at: u64,
    grace_ready_at: u64,
}

pub(super) fn sql_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap()
}

fn sql_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap()
}

#[derive(Debug, Clone, Default)]
struct CooperativeObservations {
    prefetch_count: usize,
    authorized_count: usize,
    close_channel_id: Option<String>,
    close_channel_ids: Vec<String>,
    close_cumulative_amount: Option<String>,
    close_signature: Option<String>,
    credential_source: Option<String>,
}

#[derive(Clone, Default)]
struct CooperativeCloseConfig {
    auth_failure_once_status: Option<StatusCode>,
}

#[derive(Clone)]
struct CooperativeCloseState {
    realm: String,
    observations: Arc<Mutex<CooperativeObservations>>,
    fail_next_authorized: Arc<Mutex<Option<StatusCode>>>,
}

struct CooperativeCloseServer {
    base_url: String,
    observations: Arc<Mutex<CooperativeObservations>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl CooperativeCloseServer {
    async fn start() -> Self {
        Self::start_with_config(CooperativeCloseConfig::default()).await
    }

    async fn start_with_config(config: CooperativeCloseConfig) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}:{}", addr.ip(), addr.port());
        let realm = format!("{}:{}", addr.ip(), addr.port());

        let observations = Arc::new(Mutex::new(CooperativeObservations::default()));
        let fail_next_authorized = Arc::new(Mutex::new(config.auth_failure_once_status));
        let state = CooperativeCloseState {
            realm,
            observations: observations.clone(),
            fail_next_authorized,
        };

        let app = Router::new()
            .route("/close", post(cooperative_close_handler))
            .with_state(state);
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
            observations,
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
        }
    }

    fn close_url(&self) -> String {
        format!("{}/close", self.base_url)
    }

    fn snapshot(&self) -> CooperativeObservations {
        self.observations.lock().unwrap().clone()
    }
}

impl Drop for CooperativeCloseServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn cooperative_close_handler(
    State(state): State<CooperativeCloseState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(auth_value) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    else {
        let request = Base64UrlJson::from_value(&json!({})).unwrap();
        let challenge =
            PaymentChallenge::new("it-close", &state.realm, "tempo", "session", request);
        let www_authenticate = mpp::format_www_authenticate(&challenge).unwrap();
        let mut observations = state.observations.lock().unwrap();
        observations.prefetch_count += 1;
        return Response::builder()
            .status(StatusCode::PAYMENT_REQUIRED)
            .header("www-authenticate", www_authenticate)
            .body(Body::empty())
            .unwrap();
    };

    let parsed = mpp::parse_authorization(auth_value).unwrap();
    let payload: mpp::protocol::methods::tempo::session::SessionCredentialPayload =
        parsed.payload_as().unwrap();
    let mut observations = state.observations.lock().unwrap();
    observations.authorized_count += 1;
    observations.credential_source = parsed.source;
    if let mpp::protocol::methods::tempo::session::SessionCredentialPayload::Close {
        channel_id,
        cumulative_amount,
        signature,
        ..
    } = payload
    {
        observations.close_channel_id = Some(channel_id.clone());
        observations.close_channel_ids.push(channel_id);
        observations.close_cumulative_amount = Some(cumulative_amount);
        observations.close_signature = Some(signature);
    }

    if let Some(status) = state.fail_next_authorized.lock().unwrap().take() {
        let body = json!({
            "type": "https://paymentauth.org/problems/internal-error",
            "title": "Internal Server Error",
            "status": status.as_u16(),
        });
        return Response::builder()
            .status(status)
            .body(Body::from(body.to_string()))
            .unwrap();
    }

    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("closed"))
        .unwrap()
}

#[derive(Debug, Clone, Copy)]
struct RpcCloseConfig {
    close_requested_at: u64,
    finalized: bool,
    grace_period: u64,
    orphaned_log_channel_id: Option<&'static str>,
}

#[derive(Debug, Clone, Default)]
struct RpcCloseObservations {
    send_raw_count: usize,
}

struct CloseRpcServer {
    base_url: String,
    observations: Arc<Mutex<RpcCloseObservations>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl CloseRpcServer {
    async fn start(config: RpcCloseConfig) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}:{}", addr.ip(), addr.port());

        let observations = Arc::new(Mutex::new(RpcCloseObservations::default()));
        let app = Router::new().route(
            "/",
            post({
                let observations = observations.clone();
                move |Json(body): Json<serde_json::Value>| {
                    let observations = observations.clone();
                    async move {
                        let response = if let Some(batch) = body.as_array() {
                            serde_json::Value::Array(
                                batch
                                    .iter()
                                    .map(|request| {
                                        rpc_close_response(request, &config, &observations)
                                    })
                                    .collect(),
                            )
                        } else {
                            rpc_close_response(&body, &config, &observations)
                        };
                        Json(response)
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
            observations,
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
        }
    }

    fn snapshot(&self) -> RpcCloseObservations {
        self.observations.lock().unwrap().clone()
    }
}

impl Drop for CloseRpcServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn rpc_close_response(
    request: &serde_json::Value,
    config: &RpcCloseConfig,
    observations: &Arc<Mutex<RpcCloseObservations>>,
) -> serde_json::Value {
    let method = request["method"].as_str().unwrap_or("");
    let id = request["id"].clone();
    let result = match method {
        "eth_chainId" => json!("0xa5bf"), // 42431
        "eth_getTransactionCount" => json!("0x0"),
        "eth_blockNumber" => json!("0x100000"),
        "eth_estimateGas" => json!("0x5208"),
        "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
        "eth_gasPrice" => json!("0x4a817c800"),
        "eth_getBalance" => json!("0xde0b6b3a7640000"),
        "eth_getLogs" => {
            if let Some(channel_id) = config.orphaned_log_channel_id {
                json!([channel_opened_log(channel_id)])
            } else {
                json!([])
            }
        }
        "eth_call" => {
            let call_input = request["params"]
                .get(0)
                .and_then(|tx| tx.get("input").or_else(|| tx.get("data")))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("0x");
            if call_input.len() <= 10 {
                json!(format!(
                    "0x{}",
                    hex::encode(encode_u64_word(config.grace_period))
                ))
            } else {
                json!(encode_get_channel_return_data(
                    config.close_requested_at,
                    config.finalized
                ))
            }
        }
        "eth_sendRawTransaction" => {
            let mut guard = observations.lock().unwrap();
            guard.send_raw_count += 1;
            json!("0x0000000000000000000000000000000000000000000000000000000000000001")
        }
        "eth_getBlockByNumber" => {
            let zeros = "0".repeat(512);
            json!({
                "number": "0x1",
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "baseFeePerGas": "0x3b9aca00",
                "timestamp": "0x60000000",
                "gasLimit": "0x1c9c380",
                "gasUsed": "0x0",
                "miner": "0x0000000000000000000000000000000000000000",
                "difficulty": "0x0",
                "totalDifficulty": "0x0",
                "extraData": "0x",
                "size": "0x100",
                "nonce": "0x0000000000000000",
                "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
                "logsBloom": format!("0x{zeros}"),
                "transactionsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
                "stateRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
                "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
                "transactions": [],
                "uncles": []
            })
        }
        _ => serde_json::Value::Null,
    };

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn encode_get_channel_return_data(close_requested_at: u64, finalized: bool) -> String {
    let payer: alloy::primitives::Address = SEEDED_PAYER.parse().unwrap();
    let payee: alloy::primitives::Address = "0x0000000000000000000000000000000000000002"
        .parse()
        .unwrap();
    let token: alloy::primitives::Address = MODERATO_TOKEN.parse().unwrap();
    let authorized_signer: alloy::primitives::Address = SEEDED_PAYER.parse().unwrap();

    // New ABI order: (finalized, closeRequestedAt, payer, payee, token, authorizedSigner, deposit, settled)
    let mut encoded = Vec::with_capacity(32 * 8);
    encoded.extend(encode_bool_word(finalized));
    encoded.extend(encode_u64_word(close_requested_at));
    encoded.extend(encode_address_word(payer));
    encoded.extend(encode_address_word(payee));
    encoded.extend(encode_address_word(token));
    encoded.extend(encode_address_word(authorized_signer));
    encoded.extend(encode_u128_word(10_000_000));
    encoded.extend(encode_u128_word(0));
    format!("0x{}", hex::encode(encoded))
}

fn encode_address_word(address: alloy::primitives::Address) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(address.as_slice());
    out
}

fn encode_u128_word(value: u128) -> [u8; 32] {
    alloy::primitives::U256::from(value).to_be_bytes::<32>()
}

fn encode_u64_word(value: u64) -> [u8; 32] {
    alloy::primitives::U256::from(value).to_be_bytes::<32>()
}

fn encode_bool_word(value: bool) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[31] = u8::from(value);
    out
}

fn topic_from_address(address: alloy::primitives::Address) -> String {
    format!("0x{}", hex::encode(encode_address_word(address)))
}

fn channel_opened_log(channel_id: &str) -> serde_json::Value {
    let payer: alloy::primitives::Address = SEEDED_PAYER.parse().unwrap();
    let payee: alloy::primitives::Address = "0x0000000000000000000000000000000000000002"
        .parse()
        .unwrap();
    let token: alloy::primitives::Address = MODERATO_TOKEN.parse().unwrap();

    let mut data = Vec::with_capacity(96);
    data.extend(encode_address_word(token));
    data.extend(encode_u128_word(10_000_000));
    data.extend([0u8; 32]);

    json!({
        "removed": false,
        "address": TEMPO_MODERATO_ESCROW.to_string(),
        "data": format!("0x{}", hex::encode(data)),
        "topics": [
            CHANNEL_OPENED_TOPIC,
            channel_id,
            topic_from_address(payer),
            topic_from_address(payee),
        ],
        "blockNumber": "0x100000",
        "transactionHash": "0x0000000000000000000000000000000000000000000000000000000000000002",
        "transactionIndex": "0x0",
        "blockHash": "0x0000000000000000000000000000000000000000000000000000000000000003",
        "logIndex": "0x0"
    })
}

fn seed_session_for_close(
    temp: &tempfile::TempDir,
    origin: &str,
    request_url: &str,
    cumulative_amount: u128,
) {
    seed_local_session(temp, origin);
    let challenge_request = Base64UrlJson::from_value(&json!({})).unwrap();
    let challenge = PaymentChallenge::new(
        "it-close",
        "close.test",
        "tempo",
        "session",
        challenge_request,
    );
    let challenge_echo = serde_json::to_string(&challenge.to_echo()).unwrap();
    let db_path = temp.path().join(".tempo/wallet/channels.db");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE channels
         SET request_url = ?1,
             chain_id = 42431,
             escrow_contract = ?2,
             token = ?3,
             payer = ?4,
             authorized_signer = ?4,
             cumulative_amount = ?5,
             challenge_echo = ?6,
             state = 'active',
             close_requested_at = 0,
             grace_ready_at = 0
         WHERE channel_id = ?7",
        rusqlite::params![
            request_url,
            TEMPO_MODERATO_ESCROW.to_string(),
            MODERATO_TOKEN,
            SEEDED_PAYER,
            cumulative_amount.to_string(),
            challenge_echo,
            SEEDED_CHANNEL_ID,
        ],
    )
    .unwrap();
}

fn insert_session_for_close(
    temp: &tempfile::TempDir,
    channel_id: &str,
    origin: &str,
    request_url: &str,
    cumulative_amount: u128,
) {
    let challenge_request = Base64UrlJson::from_value(&json!({})).unwrap();
    let challenge = PaymentChallenge::new(
        "it-close",
        "close.test",
        "tempo",
        "session",
        challenge_request,
    );
    let challenge_echo = serde_json::to_string(&challenge.to_echo()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let db_path = temp.path().join(".tempo/wallet/channels.db");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO channels (
            channel_id,
            version,
            origin,
            request_url,
            chain_id,
            escrow_contract,
            token,
            payee,
            payer,
            authorized_signer,
            salt,
            deposit,
            cumulative_amount,
            challenge_echo,
            state,
            close_requested_at,
            grace_ready_at,
            created_at,
            last_used_at
        ) VALUES (?1, 1, ?2, ?3, 42431, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10, ?11, 'active', 0, 0, ?12, ?12)",
        rusqlite::params![
            channel_id,
            origin,
            request_url,
            TEMPO_MODERATO_ESCROW.to_string(),
            MODERATO_TOKEN,
            "0x0000000000000000000000000000000000000002",
            SEEDED_PAYER,
            "0x00",
            "1000000",
            cumulative_amount.to_string(),
            challenge_echo,
            sql_i64(now),
        ],
    )
    .unwrap();
}

fn read_close_state(temp: &tempfile::TempDir) -> Option<StoredCloseState> {
    read_close_state_for(temp, SEEDED_CHANNEL_ID)
}

fn read_close_state_for(temp: &tempfile::TempDir, channel_id: &str) -> Option<StoredCloseState> {
    let db_path = temp.path().join(".tempo/wallet/channels.db");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT state, close_requested_at, grace_ready_at
         FROM channels
         WHERE channel_id = ?1",
        [channel_id],
        |row| {
            let close_requested_at = row.get::<_, i64>(1)?;
            let grace_ready_at = row.get::<_, i64>(2)?;
            Ok(StoredCloseState {
                state: row.get(0)?,
                close_requested_at: sql_u64(close_requested_at),
                grace_ready_at: sql_u64(grace_ready_at),
            })
        },
    )
    .ok()
}

fn session_row_count(temp: &tempfile::TempDir) -> u64 {
    let db_path = temp.path().join(".tempo/wallet/channels.db");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM channels", [], |row| {
        Ok(sql_u64(row.get::<_, i64>(0)?))
    })
    .unwrap()
}

// ==================== sessions ====================

mod close;
mod list;
mod sync;
