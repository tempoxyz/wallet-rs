//! Mock servers for integration tests: HTTP, JSON-RPC, and MPP service directory.

use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{any, get},
    Json, Router,
};
use serde_json::json;

// ── Generic HTTP mock ───────────────────────────────────────────────────

/// Generic mock HTTP server backed by axum.
pub struct MockServer {
    pub base_url: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _handle: tokio::task::JoinHandle<()>,
    /// Deferred www-authenticate header (set after server binds).
    www_auth_tx: Option<std::sync::Arc<tokio::sync::watch::Sender<String>>>,
    /// Authorization header values captured from paid requests.
    captured_auth: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockServer {
    /// Start a server that always returns the given status, headers, and body.
    ///
    /// # Panics
    ///
    /// Panics when binding or running the mock server fails.
    pub async fn start(status: u16, headers: Vec<(&str, &str)>, body: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let status_code = StatusCode::from_u16(status).unwrap();
        let owned_headers: Vec<(String, String)> = headers
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let owned_body = body.to_string();

        let app = Router::new().route(
            "/{*path}",
            any(move || {
                let hdrs = owned_headers.clone();
                let b = owned_body.clone();
                async move {
                    let mut response = (status_code, b).into_response();
                    for (k, v) in &hdrs {
                        response.headers_mut().insert(
                            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                            axum::http::HeaderValue::from_str(v).unwrap(),
                        );
                    }
                    response
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
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
            www_auth_tx: None,
            captured_auth: Default::default(),
        }
    }

    /// Start a payment mock: returns 402 + WWW-Authenticate when no Authorization
    /// header is present, returns 200 + body when Authorization header is present.
    ///
    /// # Panics
    ///
    /// Panics when binding or running the mock server fails.
    pub async fn start_payment(www_authenticate: &str, success_body: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let owned_header = www_authenticate.to_string();
        let owned_body = success_body.to_string();

        let app = Router::new().route(
            "/{*path}",
            any(move |headers: axum::http::HeaderMap| {
                let h = owned_header.clone();
                let b = owned_body.clone();
                async move {
                    if headers.get("authorization").is_some() {
                        (StatusCode::OK, b).into_response()
                    } else {
                        let mut response =
                            (StatusCode::PAYMENT_REQUIRED, "Payment Required").into_response();
                        response.headers_mut().insert(
                            axum::http::HeaderName::from_static("www-authenticate"),
                            axum::http::HeaderValue::from_str(&h).unwrap(),
                        );
                        response
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
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
            www_auth_tx: None,
            captured_auth: Default::default(),
        }
    }

    /// Start a payment mock that also returns a Payment-Receipt header on success.
    ///
    /// # Panics
    ///
    /// Panics when binding or running the mock server fails.
    pub async fn start_payment_with_receipt(
        www_authenticate: &str,
        success_body: &str,
        receipt_header: &str,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let owned_header = www_authenticate.to_string();
        let owned_body = success_body.to_string();
        let owned_receipt = receipt_header.to_string();

        let app = Router::new().route(
            "/{*path}",
            any(move |headers: axum::http::HeaderMap| {
                let h = owned_header.clone();
                let b = owned_body.clone();
                let r = owned_receipt.clone();
                async move {
                    if headers.get("authorization").is_some() {
                        let mut resp = (StatusCode::OK, b).into_response();
                        resp.headers_mut().insert(
                            axum::http::HeaderName::from_static("payment-receipt"),
                            axum::http::HeaderValue::from_str(&r).unwrap(),
                        );
                        resp
                    } else {
                        let mut response =
                            (StatusCode::PAYMENT_REQUIRED, "Payment Required").into_response();
                        response.headers_mut().insert(
                            axum::http::HeaderName::from_static("www-authenticate"),
                            axum::http::HeaderValue::from_str(&h).unwrap(),
                        );
                        response
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
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
            www_auth_tx: None,
            captured_auth: Default::default(),
        }
    }

    /// Set the WWW-Authenticate header for a deferred payment mock.
    pub fn set_www_authenticate(&self, value: &str) {
        if let Some(tx) = &self.www_auth_tx {
            let _ = tx.send(value.to_string());
        }
    }

    /// Start a payment mock where the WWW-Authenticate header is set after binding.
    ///
    /// Call [`Self::set_www_authenticate`] after construction to provide the header.
    pub async fn start_payment_deferred(success_body: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let (watch_tx, watch_rx) = tokio::sync::watch::channel(String::new());
        let watch_tx = std::sync::Arc::new(watch_tx);
        let owned_body = success_body.to_string();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let cap = captured.clone();

        let app = Router::new().route(
            "/{*path}",
            any(move |headers: axum::http::HeaderMap| {
                let rx = watch_rx.clone();
                let b = owned_body.clone();
                let cap = cap.clone();
                async move {
                    if let Some(auth) = headers.get("authorization") {
                        if let Ok(v) = auth.to_str() {
                            cap.lock().unwrap().push(v.to_string());
                        }
                        (StatusCode::OK, b).into_response()
                    } else {
                        let h = rx.borrow().clone();
                        let mut response =
                            (StatusCode::PAYMENT_REQUIRED, "Payment Required").into_response();
                        response.headers_mut().insert(
                            axum::http::HeaderName::from_static("www-authenticate"),
                            axum::http::HeaderValue::from_str(&h).unwrap(),
                        );
                        response
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
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
            www_auth_tx: Some(watch_tx),
            captured_auth: captured,
        }
    }

    /// Start a deferred payment mock and delay paid responses by `success_delay_ms`.
    ///
    /// This is useful for deterministic overlap in concurrency tests.
    pub async fn start_payment_deferred_with_delay(
        success_body: &str,
        success_delay_ms: u64,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let (watch_tx, watch_rx) = tokio::sync::watch::channel(String::new());
        let watch_tx = std::sync::Arc::new(watch_tx);
        let owned_body = success_body.to_string();

        let app = Router::new().route(
            "/{*path}",
            any(move |headers: axum::http::HeaderMap| {
                let rx = watch_rx.clone();
                let b = owned_body.clone();
                async move {
                    if headers.get("authorization").is_some() {
                        if success_delay_ms > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(success_delay_ms))
                                .await;
                        }
                        (StatusCode::OK, b).into_response()
                    } else {
                        let h = rx.borrow().clone();
                        let mut response =
                            (StatusCode::PAYMENT_REQUIRED, "Payment Required").into_response();
                        response.headers_mut().insert(
                            axum::http::HeaderName::from_static("www-authenticate"),
                            axum::http::HeaderValue::from_str(&h).unwrap(),
                        );
                        response
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
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
            www_auth_tx: Some(watch_tx),
            captured_auth: Default::default(),
        }
    }

    /// Start a deferred payment mock that also returns a Payment-Receipt header.
    pub async fn start_payment_deferred_with_receipt(
        success_body: &str,
        receipt_header: &str,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let (watch_tx, watch_rx) = tokio::sync::watch::channel(String::new());
        let watch_tx = std::sync::Arc::new(watch_tx);
        let owned_body = success_body.to_string();
        let owned_receipt = receipt_header.to_string();

        let app = Router::new().route(
            "/{*path}",
            any(move |headers: axum::http::HeaderMap| {
                let rx = watch_rx.clone();
                let b = owned_body.clone();
                let r = owned_receipt.clone();
                async move {
                    if headers.get("authorization").is_some() {
                        let mut resp = (StatusCode::OK, b).into_response();
                        resp.headers_mut().insert(
                            axum::http::HeaderName::from_static("payment-receipt"),
                            axum::http::HeaderValue::from_str(&r).unwrap(),
                        );
                        resp
                    } else {
                        let h = rx.borrow().clone();
                        let mut response =
                            (StatusCode::PAYMENT_REQUIRED, "Payment Required").into_response();
                        response.headers_mut().insert(
                            axum::http::HeaderName::from_static("www-authenticate"),
                            axum::http::HeaderValue::from_str(&h).unwrap(),
                        );
                        response
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
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
            www_auth_tx: Some(watch_tx),
            captured_auth: Default::default(),
        }
    }

    /// Start a mock that echoes request headers back as a JSON body.
    ///
    /// # Panics
    ///
    /// Panics when binding or running the mock server fails.
    pub async fn start_echo_headers() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let app = Router::new().route(
            "/{*path}",
            any(move |headers: axum::http::HeaderMap| async move {
                let mut map = serde_json::Map::new();
                for (k, v) in &headers {
                    if let Ok(s) = v.to_str() {
                        map.insert(
                            k.as_str().to_string(),
                            serde_json::Value::String(s.to_string()),
                        );
                    }
                }
                let body = serde_json::to_string(&map).unwrap();
                (StatusCode::OK, body).into_response()
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
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
            www_auth_tx: None,
            captured_auth: Default::default(),
        }
    }

    /// Start a mock that echoes back the full request as JSON:
    /// `{ "method": "...", "path": "...", "query": "...", "headers": {...}, "body": "..." }`
    ///
    /// # Panics
    ///
    /// Panics when binding or running the mock server fails.
    pub async fn start_echo_request() -> Self {
        use axum::http::Request;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let app = Router::new().route(
            "/{*path}",
            any(move |req: Request<axum::body::Body>| async move {
                let method = req.method().to_string();
                let path = req.uri().path().to_string();
                let query = req.uri().query().unwrap_or("").to_string();
                let mut hdr_map = serde_json::Map::new();
                for (k, v) in req.headers() {
                    if let Ok(s) = v.to_str() {
                        hdr_map.insert(
                            k.as_str().to_string(),
                            serde_json::Value::String(s.to_string()),
                        );
                    }
                }
                let body_bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                    .await
                    .unwrap_or_default();
                let body_str = String::from_utf8_lossy(&body_bytes).to_string();
                let obj = serde_json::json!({
                    "method": method,
                    "path": path,
                    "query": query,
                    "headers": hdr_map,
                    "body": body_str,
                });
                (StatusCode::OK, serde_json::to_string(&obj).unwrap()).into_response()
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
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
            www_auth_tx: None,
            captured_auth: Default::default(),
        }
    }

    /// Start a mock that returns an SSE stream with the given raw body.
    ///
    /// # Panics
    ///
    /// Panics when binding or running the mock server fails.
    pub async fn start_sse(body: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let owned_body = body.to_string();

        let app = Router::new().route(
            "/{*path}",
            any(move || {
                let b = owned_body.clone();
                async move {
                    let mut response = (StatusCode::OK, b).into_response();
                    response.headers_mut().insert(
                        axum::http::HeaderName::from_static("content-type"),
                        axum::http::HeaderValue::from_static("text/event-stream"),
                    );
                    response
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
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
            www_auth_tx: None,
            captured_auth: Default::default(),
        }
    }

    /// Returns Authorization header values captured from paid requests.
    pub fn captured_auth_headers(&self) -> Vec<String> {
        self.captured_auth.lock().unwrap().clone()
    }

    /// Get the full URL for a path on this server.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

// ── JSON-RPC mock ───────────────────────────────────────────────────────

/// Mock JSON-RPC server that responds to standard EVM methods.
pub struct MockRpcServer {
    pub base_url: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockRpcServer {
    /// Start a mock RPC server for the given chain ID.
    ///
    /// # Panics
    ///
    /// Panics when binding or running the mock server fails.
    pub async fn start(chain_id: u64) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        let app = Router::new().route(
            "/",
            axum::routing::post(
                move |axum::extract::Json(body): axum::extract::Json<serde_json::Value>| async move {
                    let response = if body.is_array() {
                        serde_json::Value::Array(
                            body.as_array()
                                .unwrap()
                                .iter()
                                .map(|req| mock_rpc_response(req, chain_id))
                                .collect(),
                        )
                    } else {
                        mock_rpc_response(&body, chain_id)
                    };
                    axum::Json(response)
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
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
        }
    }
}

impl Drop for MockRpcServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Generate a mock JSON-RPC response for a given method.
#[must_use]
pub fn mock_rpc_response(req: &serde_json::Value, chain_id: u64) -> serde_json::Value {
    let method = req["method"].as_str().unwrap_or("");
    let id = req["id"].clone();

    let result: serde_json::Value = match method {
        "eth_chainId" => json!(format!("0x{:x}", chain_id)),
        "eth_getTransactionCount" => json!("0x0"),
        "eth_estimateGas" => json!("0x5208"),
        "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
        "eth_gasPrice" => json!("0x4a817c800"),
        "eth_getBalance" => json!("0xde0b6b3a7640000"),
        "eth_call" => json!("0x"),
        "eth_sendRawTransaction" => {
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
        "result": result
    })
}

// ── MPP service directory mock ──────────────────────────────────────────

/// Mock MPP service directory server.
pub struct MockServicesServer {
    pub services_url: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServicesServer {
    /// Start a mock services directory with a default payload.
    pub async fn start() -> Self {
        Self::start_with_payload(json!({
            "services": [
                {
                    "id": "openai",
                    "name": "OpenAI",
                    "url": "https://openrouter.mpp.tempo.xyz",
                    "serviceUrl": "https://openrouter.mpp.tempo.xyz/v1/chat/completions",
                    "supportsCredits": true,
                    "description": "LLM API",
                    "categories": ["ai"],
                    "methods": {"tempo": {"intents": ["charge"]}}
                }
            ]
        }))
        .await
    }

    /// Start a mock services directory with a custom payload.
    ///
    /// # Panics
    ///
    /// Panics when binding or running the mock server fails.
    pub async fn start_with_payload(payload: serde_json::Value) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let services_url = format!("http://{addr}/services");

        let app = Router::new().route(
            "/services",
            get(move || {
                let p = payload.clone();
                async move { Json(p) }
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
            services_url,
            shutdown_tx: Some(shutdown_tx),
            _handle: handle,
        }
    }
}

impl Drop for MockServicesServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}
