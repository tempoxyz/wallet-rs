//! Payment and private-key flow tests split from commands.rs.

use super::*;

fn run_two_concurrent_charge_requests(
    temp: &tempfile::TempDir,
    url: &str,
) -> (std::process::Output, std::process::Output) {
    std::thread::scope(|scope| {
        let first = scope.spawn(|| test_command(temp).args(["-v", url]).output().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(25));
        let second = scope.spawn(|| test_command(temp).args(["-v", url]).output().unwrap());
        (first.join().unwrap(), second.join().unwrap())
    })
}

// ==================== 402 Charge Payment Flow ====================

/// Test the full 402 → payment → 200 charge flow with mock servers.
///
/// Verifies that tempo-request correctly:
/// 1. Receives a 402 with WWW-Authenticate header
/// 2. Parses the MPP payment challenge
/// 3. Loads wallet keys
/// 4. Builds and signs a payment credential via mock RPC
/// 5. Retries the request with Authorization header
/// 6. Returns the final 200 response body
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_charge_flow() {
    let h = PaymentTestHarness::charge_with_body("charge accepted").await;

    let output = test_command(&h.temp)
        .args(["-v", &h.url("/api")])
        .output()
        .unwrap();

    assert_success(&output, "expected 402 → payment → 200 flow to succeed");
    assert_stdout_contains(
        &output,
        "charge accepted",
        "stdout should contain success body",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_charge_flow_respects_max_spend() {
    let h = PaymentTestHarness::charge_with_body("charge accepted").await;

    let output = test_command(&h.temp)
        .args(["--max-spend", "0.0005", &h.url("/api")])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "expected charge flow to fail when --max-spend is below challenge amount: {}",
        get_combined_output(&output)
    );
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("Payment max spend exceeded"),
        "error should describe max-spend rejection: {combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_charge_flow_respects_max_spend_env() {
    let h = PaymentTestHarness::charge_with_body("charge accepted").await;

    let output = test_command(&h.temp)
        .env("TEMPO_MAX_SPEND", "0.0005")
        .arg(h.url("/api"))
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "expected charge flow to fail when TEMPO_MAX_SPEND is below challenge amount: {}",
        get_combined_output(&output)
    );
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("Payment max spend exceeded"),
        "error should describe max-spend rejection from env var: {combined}"
    );
}

/// Concurrent same-origin charge requests serialize correctly and avoid nonce replay failures.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_charge_flow_concurrent_same_origin_no_nonce_replay() {
    let rpc = MockRpcServer::start(42431).await;
    let server = MockServer::start_payment_deferred_with_delay("charge accepted", 250).await;
    let www_auth = charge_www_authenticate_with_realm("test-charge-concurrent", &server.base_url);
    server.set_www_authenticate(&www_auth);

    let temp = TestConfigBuilder::new()
        .with_keys_toml(tempo_test::MODERATO_DIRECT_KEYS_TOML)
        .with_config_toml(format!(
            "[rpc]\n\"tempo-moderato\" = \"{}\"\n",
            rpc.base_url
        ))
        .build();

    let request_url = server.url("/api");
    let (first_output, second_output) = run_two_concurrent_charge_requests(&temp, &request_url);

    assert_success(
        &first_output,
        "first concurrent charge request should succeed",
    );
    assert_success(
        &second_output,
        "second concurrent charge request should succeed",
    );

    let first_combined = get_combined_output(&first_output).to_lowercase();
    let second_combined = get_combined_output(&second_output).to_lowercase();
    for combined in [first_combined, second_combined] {
        assert!(
            !combined.contains("already known"),
            "concurrent charge run should not surface nonce replay signature 'already known': {combined}"
        );
        assert!(
            !combined.contains("expiring nonce"),
            "concurrent charge run should not surface expiring nonce replay signatures: {combined}"
        );
    }
}

/// Missing receipts remain warning-only in default mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_charge_flow_default_mode_allows_missing_receipt() {
    let h = PaymentTestHarness::charge_with_body("default receipt policy ok").await;

    let output = test_command(&h.temp)
        .args([&h.url("/api")])
        .output()
        .unwrap();

    assert_success(&output, "default mode should not fail on missing receipt");
}

/// Payment narration is printed at -v when encountering a 402
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_payment_narration_verbose() {
    let h = PaymentTestHarness::charge().await;

    let output = test_command(&h.temp)
        .args(["-v", &h.url("/api")])
        .output()
        .unwrap();

    assert_success(&output, "402 narration verbose flow should succeed");
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("payment required:"),
        "should narrate 402 payment requirement: {stderr}"
    );
}

/// Paid summary prints with -v and is suppressed by default / -q
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_paid_summary_verbose_and_quiet() {
    let h = PaymentTestHarness::charge_with_id("test-charge-paid", "ok").await;

    // Default (no -v): summary should NOT be printed
    let output_default = test_command(&h.temp)
        .args([&h.url("/api")])
        .output()
        .unwrap();
    assert!(
        output_default.status.success(),
        "default run should succeed"
    );
    let stderr_default = String::from_utf8_lossy(&output_default.stderr);
    assert!(
        !stderr_default.contains("Paid "),
        "expected no paid summary in default mode, got: {stderr_default}"
    );

    // Verbose: summary should be printed
    let output_verbose = test_command(&h.temp)
        .args(["-v", &h.url("/api")])
        .output()
        .unwrap();
    assert!(
        output_verbose.status.success(),
        "verbose run should succeed"
    );
    let stderr_verbose = String::from_utf8_lossy(&output_verbose.stderr);
    assert!(
        stderr_verbose.contains("Paid "),
        "expected paid summary in verbose mode, got: {stderr_verbose}"
    );

    // Quiet: summary should be suppressed
    let output_quiet = test_command(&h.temp)
        .args(["-s", &h.url("/api")])
        .output()
        .unwrap();
    assert!(output_quiet.status.success(), "quiet run should succeed");
    let stderr_quiet = String::from_utf8_lossy(&output_quiet.stderr);
    assert!(
        !stderr_quiet.contains("Paid "),
        "expected no paid summary in quiet mode, got: {stderr_quiet}"
    );
}

/// Analytics `PaymentSuccess` `tx_hash` should be the extracted hex, not the raw header
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn analytics_tx_hash_is_extracted_hex() {
    // Simple 64-nybble hex hash
    let tx_hash = format!("0x{}", "ab".repeat(32));
    let receipt_value = format!("tx={tx_hash}");
    let h = PaymentTestHarness::charge_with_receipt("ok", &receipt_value).await;

    // Set up analytics tap file
    let events_path = h.temp.path().join("events.log");

    let output = test_command(&h.temp)
        .env("TEMPO_TEST_EVENTS", events_path.to_str().unwrap())
        .args(["-v", &h.url("/api")])
        .output()
        .unwrap();
    assert!(output.status.success(), "expected success");

    // Read captured events and find payment succeeded
    let content = std::fs::read_to_string(&events_path).unwrap();
    let mut found = None;
    for line in content.lines() {
        if let Some((name, json_str)) = line.split_once('|') {
            if name == "payment succeeded" {
                found = Some(json_str.to_string());
            }
        }
    }
    let Some(json_str) = found else {
        panic!("missing payment succeeded event: {content}");
    };
    let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let got = v.get("tx_hash").and_then(|x| x.as_str()).unwrap_or("");
    // Accept either an exact extracted 0x-hash, or empty (if server didn't supply a
    // parseable receipt). Critically, it must NOT be the raw header with fields.
    let is_hex =
        got.starts_with("0x") && got.len() == 66 && got[2..].chars().all(|c| c.is_ascii_hexdigit());
    assert!(
        got.is_empty() || is_hex,
        "tx_hash should be empty or a 0x-hex hash, got: {got}"
    );
    assert!(
        !got.contains('='),
        "tx_hash should not be a raw header with fields: {got}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paid_success_analytics_redacts_url_query_params() {
    let h = PaymentTestHarness::charge_with_body("paid analytics ok").await;
    let events_path = h.temp.path().join("paid_events_url_redact.log");
    let url_with_secrets = h.url("/api?api_key=sk_live_paid_secret&token=paid_bearer_secret");

    let output = test_command(&h.temp)
        .env("TEMPO_TEST_EVENTS", events_path.to_str().unwrap())
        .args(["--no-proxy", "--dry-run", &url_with_secrets])
        .output()
        .unwrap();
    assert_success(&output, "paid dry-run request should succeed");

    let events = parse_events_log(&events_path);
    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"payment succeeded"),
        "missing payment succeeded event: {names:?}"
    );
    assert!(
        names.contains(&"query succeeded"),
        "missing query succeeded event: {names:?}"
    );

    for (name, props) in &events {
        if let Some(url) = props.get("url").and_then(|value| value.as_str()) {
            assert!(
                !url.contains("sk_live_paid_secret"),
                "event '{name}' leaks api_key in url: {url}"
            );
            assert!(
                !url.contains("paid_bearer_secret"),
                "event '{name}' leaks token in url: {url}"
            );
            assert!(
                !url.contains('?'),
                "event '{name}' has query params in url: {url}"
            );
        }
    }
}

/// Test the 402 → payment → 200 charge flow with Keychain signing mode.
///
/// Uses a different `wallet_address` than the derived address of the private
/// key, which triggers `TempoSigningMode::Keychain` instead of `Direct`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_charge_flow_keychain() {
    let h = PaymentTestHarness::charge_keychain("keychain charge accepted").await;

    let output = test_command(&h.temp)
        .args(["-v", &h.url("/api")])
        .output()
        .unwrap();

    let combined = get_combined_output(&output);
    assert!(
        output.status.success(),
        "expected keychain 402 → payment → 200 flow to succeed: {combined}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("keychain charge accepted"),
        "stdout should contain success body: {combined}"
    );
}

// ==================== Client ID Attribution ====================

/// Charge flow sends an Authorization credential whose embedded transaction
/// contains the "tempo-wallet" client fingerprint in the MPP attribution memo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_charge_flow_sends_client_id_in_credential() {
    let h = PaymentTestHarness::charge_with_body("client id ok").await;

    let output = test_command(&h.temp)
        .args([&h.url("/api")])
        .output()
        .unwrap();

    assert_success(&output, "charge flow with client id should succeed");

    let auth_headers = h.server.captured_auth_headers();
    assert!(
        !auth_headers.is_empty(),
        "expected at least one Authorization header from the paid request"
    );

    // The credential contains the signed transaction which includes the
    // keccak256("tempo-wallet")[0..10] fingerprint in the attribution memo.
    let client_fingerprint = {
        let hash = alloy::primitives::keccak256(b"tempo-wallet");
        hex::encode(&hash[..10])
    };
    // The credential is "Payment <base64-json>" where the JSON contains
    // hex-encoded transaction bytes with the attribution memo inside.
    let header = &auth_headers[0];
    let payload_b64 = header.strip_prefix("Payment ").unwrap_or(header);
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        payload_b64,
    )
    .or_else(|_| {
        base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            payload_b64,
        )
    })
    .expect("credential should be valid base64");
    let decoded_str = String::from_utf8_lossy(&decoded);
    assert!(
        decoded_str.contains(&client_fingerprint),
        "decoded credential should contain tempo-wallet client fingerprint ({client_fingerprint})"
    );
}

// ==================== --private-key Flag ====================

/// The 402 charge flow works with --private-key (no keys.toml needed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_charge_flow_with_private_key_flag() {
    let rpc = MockRpcServer::start(42431).await;
    let server = MockServer::start_payment_deferred("private key charge ok").await;
    let www_auth = charge_www_authenticate_with_realm("test-pk", &server.base_url);
    server.set_www_authenticate(&www_auth);

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let output = test_command(&temp)
        .args([
            "-v",
            "--private-key",
            MODERATO_PRIVATE_KEY,
            &server.url("/api"),
        ])
        .output()
        .unwrap();

    let combined = get_combined_output(&output);
    assert!(
        output.status.success(),
        "expected --private-key charge flow to succeed: {combined}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("private key charge ok"),
        "stdout should contain success body: {combined}"
    );
}

/// --private-key via `TEMPO_PRIVATE_KEY` env var works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_charge_flow_with_private_key_env() {
    let rpc = MockRpcServer::start(42431).await;
    let server = MockServer::start_payment_deferred("env key charge ok").await;
    let www_auth = charge_www_authenticate_with_realm("test-pk-env", &server.base_url);
    server.set_www_authenticate(&www_auth);

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let output = test_command(&temp)
        .env("TEMPO_PRIVATE_KEY", MODERATO_PRIVATE_KEY)
        .args(["-v", &server.url("/api")])
        .output()
        .unwrap();

    let combined = get_combined_output(&output);
    assert!(
        output.status.success(),
        "expected TEMPO_PRIVATE_KEY charge flow to succeed: {combined}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("env key charge ok"),
        "stdout should contain success body: {combined}"
    );
}

/// --private-key without 0x prefix works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_key_without_0x_prefix() {
    let rpc = MockRpcServer::start(42431).await;
    let server = MockServer::start_payment_deferred("no prefix ok").await;
    let www_auth = charge_www_authenticate_with_realm("test-pk-no0x", &server.base_url);
    server.set_www_authenticate(&www_auth);

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    // Strip the 0x prefix
    let pk_no_prefix = MODERATO_PRIVATE_KEY.strip_prefix("0x").unwrap();

    let output = test_command(&temp)
        .args(["--private-key", pk_no_prefix, &server.url("/api")])
        .output()
        .unwrap();

    let combined = get_combined_output(&output);
    assert!(
        output.status.success(),
        "expected --private-key without 0x to succeed: {combined}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no prefix ok"),
        "stdout should contain success body: {combined}"
    );
}

/// --private-key with invalid hex gives a clear error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_key_invalid_hex_fails() {
    let rpc = MockRpcServer::start(42431).await;
    let server = MockServer::start_payment_deferred("should not reach").await;
    let www_auth = charge_www_authenticate_with_realm("test-pk-bad", &server.base_url);
    server.set_www_authenticate(&www_auth);

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let output = test_command(&temp)
        .args(["--private-key", "not-a-valid-key", &server.url("/api")])
        .output()
        .unwrap();

    assert_exit_code(&output, 2, "invalid private key should exit with E_USAGE");
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("Invalid private key") || combined.contains("Invalid hex"),
        "error should mention invalid key: {combined}"
    );
}

/// --private-key with too-short key gives a clear error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_key_wrong_length_fails() {
    let rpc = MockRpcServer::start(42431).await;
    let server = MockServer::start_payment_deferred("should not reach").await;
    let www_auth = charge_www_authenticate_with_realm("test-pk-short", &server.base_url);
    server.set_www_authenticate(&www_auth);

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let output = test_command(&temp)
        .args(["--private-key", "0xdeadbeef", &server.url("/api")])
        .output()
        .unwrap();

    assert_exit_code(&output, 2, "too-short private key should exit with E_USAGE");
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("Invalid private key"),
        "error should mention invalid key: {combined}"
    );
}

/// --private-key takes precedence over keys.toml (keys.toml is ignored).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_key_flag_overrides_wallet() {
    let rpc = MockRpcServer::start(42431).await;
    let server = MockServer::start_payment_deferred("override ok").await;
    let www_auth = charge_www_authenticate_with_realm("test-pk-override", &server.base_url);
    server.set_www_authenticate(&www_auth);

    // Set up keys.toml with a DIFFERENT key that points to a
    // different address. The --private-key flag should be used instead.
    let wallet_toml = r#"
[[keys]]
wallet_address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
key_address = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
"#;
    let config_toml = format!("moderato_rpc = \"{}\"\n", rpc.base_url);
    let temp = tempfile::TempDir::new().unwrap();
    write_test_files(temp.path(), &config_toml, Some(wallet_toml));

    // Snapshot keys.toml content before the run
    let keys_path = temp.path().join(".tempo/wallet/keys.toml");
    let wallet_before = std::fs::read_to_string(&keys_path).unwrap();

    let output = test_command(&temp)
        .args([
            "-v",
            "--private-key",
            MODERATO_PRIVATE_KEY,
            &server.url("/api"),
        ])
        .output()
        .unwrap();

    let combined = get_combined_output(&output);
    assert!(
        output.status.success(),
        "expected --private-key to override keys.toml: {combined}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("override ok"),
        "stdout should contain success body: {combined}"
    );

    // keys.toml must not have been modified by --private-key usage
    let wallet_after = std::fs::read_to_string(&keys_path).unwrap();
    assert_eq!(
        wallet_before, wallet_after,
        "keys.toml should not be modified when --private-key is used"
    );
}

// ==================== Zero-Amount Proof Credential Flow ====================

/// Test the full 402 → proof credential → 200 flow for zero-amount charges.
///
/// Verifies that tempo-request correctly:
/// 1. Receives a 402 with a zero-amount charge challenge
/// 2. Signs an EIP-712 proof credential (no transaction)
/// 3. Submits the proof credential and receives a 200
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_zero_amount_proof_flow() {
    let h = PaymentTestHarness::zero_amount_charge("identity verified").await;

    let output = test_command(&h.temp)
        .args(["-v", &h.url("/api")])
        .output()
        .unwrap();

    assert_success(&output, "expected zero-amount proof flow to succeed");
    assert_stdout_contains(
        &output,
        "identity verified",
        "stdout should contain success body",
    );
}

/// Zero-amount proof flow logs "proof credential" in verbose mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_zero_amount_proof_narration() {
    let h = PaymentTestHarness::zero_amount_charge("proof ok").await;

    let output = test_command(&h.temp)
        .args(["-v", &h.url("/api")])
        .output()
        .unwrap();

    assert_success(&output, "zero-amount proof narration should succeed");
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("proof credential"),
        "should narrate proof credential flow: {stderr}"
    );
}

/// Zero-amount proof flow with --private-key flag (no keys.toml).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_402_zero_amount_proof_with_private_key() {
    let rpc = MockRpcServer::start(42431).await;
    let server = MockServer::start_payment_deferred("proof pk ok").await;
    let www_auth = zero_charge_www_authenticate_with_realm("test-zero-pk", &server.base_url);
    server.set_www_authenticate(&www_auth);

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let output = test_command(&temp)
        .args([
            "-v",
            "--private-key",
            MODERATO_PRIVATE_KEY,
            &server.url("/api"),
        ])
        .output()
        .unwrap();

    let combined = get_combined_output(&output);
    assert!(
        output.status.success(),
        "expected --private-key zero-amount proof to succeed: {combined}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("proof pk ok"),
        "stdout should contain success body: {combined}"
    );
}

// ==================== --private-key Flag ====================

/// Non-402 response works fine with --private-key (key is ignored, no payment).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_key_no_payment_needed() {
    let server = MockServer::start(200, vec![], "hello no payment").await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--private-key", MODERATO_PRIVATE_KEY, &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success(), "expected success");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello no payment"),
        "stdout should contain body: {stdout}"
    );
}
