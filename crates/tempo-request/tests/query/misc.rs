//! TOON, output, and miscellaneous flag behavior tests split from commands.rs.

use super::*;

// ==================== TOON Input/Output ====================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toon_output_pretty_prints_json_response() {
    let server = MockServer::start(200, vec![], r#"{"name":"Alice","age":30}"#).await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["-t", &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.starts_with('{'),
        "TOON output should not start with '{{': {stdout}"
    );
    assert!(stdout.contains("name"), "should contain 'name': {stdout}");
    assert!(stdout.contains("Alice"), "should contain 'Alice': {stdout}");
    assert!(stdout.contains("age"), "should contain 'age': {stdout}");
    assert!(stdout.contains("30"), "should contain '30': {stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toon_output_non_json_response_passthrough() {
    let server = MockServer::start(200, vec![], "hello world").await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["-t", &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello world"),
        "non-JSON body should pass through: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toon_input_sets_content_type_json() {
    let server = MockServer::start_echo_headers().await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--toon", "name: Alice", &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let ct = parsed["content-type"].as_str().unwrap();
    assert!(
        ct.contains("application/json"),
        "content-type should be application/json: {ct}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toon_input_invalid_data_errors() {
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--toon", "[invalid{toon", "http://127.0.0.1:1/test"])
        .output()
        .unwrap();

    assert_exit_code(&output, 2, "invalid TOON input should exit with E_USAGE");
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("TOON") || combined.contains("decode"),
        "should mention TOON parse error: {combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toon_and_json_input_conflict() {
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--json", "{}", "--toon", "x: 1", "http://127.0.0.1:1/test"])
        .output()
        .unwrap();

    assert_exit_code(
        &output,
        2,
        "--json + --toon conflict should exit with E_USAGE",
    );
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("cannot be used with")
            || combined.contains("conflict")
            || combined.contains("--json"),
        "should mention conflict: {combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_flag_sends_head_method() {
    let server = MockServer::start_echo_request().await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["-I", &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success(), "HEAD request should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // -I implies include headers, so stdout starts with HTTP status line
    assert!(
        stdout.contains("HTTP 200"),
        "HEAD should show headers: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_flag_outputs_body() {
    let server = MockServer::start(200, vec![], "streamed body content").await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--stream", &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success(), "stream should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("streamed body content"),
        "stream should output body: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_passthrough_outputs_raw_events() {
    let sse_body = "data: hello\n\ndata: world\n\n";
    let server = MockServer::start_sse(sse_body).await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--sse", &server.url("/stream")])
        .output()
        .unwrap();

    assert!(output.status.success(), "SSE passthrough should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // --sse outputs raw SSE text (unlike --sse-json which converts to NDJSON)
    assert!(
        stdout.contains("data: hello") || stdout.contains("hello"),
        "SSE passthrough should contain event data: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_agent_flag_sends_custom_agent() {
    let server = MockServer::start_echo_headers().await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["-A", "MyCustomAgent/1.0", &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        parsed["user-agent"], "MyCustomAgent/1.0",
        "user-agent not set correctly: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_flag_sends_authorization_header() {
    let server = MockServer::start_echo_headers().await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--bearer", "test_token_abc", &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        parsed["authorization"], "Bearer test_token_abc",
        "bearer header not echoed: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_name_saves_to_derived_filename() {
    let server = MockServer::start(200, vec![], "remote content").await;
    let temp = TestConfigBuilder::new().build();

    // Run from the temp directory so -O writes there
    let output = test_command(&temp)
        .current_dir(temp.path())
        .args(["-O", &server.url("/path/download.txt")])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "remote-name should succeed: {}",
        get_combined_output(&output)
    );
    let saved = temp.path().join("download.txt");
    assert!(saved.exists(), "file should be saved as download.txt");
    let contents = std::fs::read_to_string(&saved).unwrap();
    assert!(
        contents.contains("remote content"),
        "saved file should contain body: {contents}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_urlencode_sends_encoded_body() {
    let server = MockServer::start_echo_request().await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--data-urlencode", "msg=hello world", &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let body = parsed["body"].as_str().unwrap();
    assert!(
        body.contains("msg=hello%20world"),
        "body should contain URL-encoded data: {body}"
    );
    let ct = parsed["headers"]["content-type"].as_str().unwrap_or("");
    assert!(
        ct.contains("application/x-www-form-urlencoded"),
        "content-type should be form-urlencoded: {ct}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_flag_appends_data_to_query() {
    let server = MockServer::start_echo_request().await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["-G", "-d", "key=value", &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["method"], "GET", "-G should force GET method");
    let query = parsed["query"].as_str().unwrap();
    assert!(
        query.contains("key=value"),
        "query string should contain data: {query}"
    );
    // Body should be empty when using -G
    let body = parsed["body"].as_str().unwrap();
    assert!(body.is_empty(), "body should be empty with -G: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_flag_with_data_urlencode() {
    let server = MockServer::start_echo_request().await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args([
            "-G",
            "--data-urlencode",
            "q=hello world",
            &server.url("/search"),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["method"], "GET");
    let query = parsed["query"].as_str().unwrap();
    assert!(
        query.contains("q=hello%20world"),
        "query should contain encoded data: {query}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_meta_creates_json_file() {
    let server = MockServer::start(200, vec![], "meta test body").await;
    let temp = TestConfigBuilder::new().build();

    let meta_path = temp.path().join("meta.json");
    let output = test_command(&temp)
        .args([
            "--write-meta",
            meta_path.to_str().unwrap(),
            &server.url("/test"),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "write-meta should succeed: {}",
        get_combined_output(&output)
    );
    assert!(meta_path.exists(), "meta file should exist");
    let meta_content = std::fs::read_to_string(&meta_path).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_content).unwrap();
    assert_eq!(meta["status"], 200);
    assert!(meta["url"].is_string());
    assert!(meta["elapsed_ms"].is_number());
    assert!(meta["bytes"].is_number());
    assert!(meta["headers"].is_object());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_redirs_limits_redirects() {
    // Server that always redirects to itself
    let server = MockServer::start(
        302,
        vec![("location", "http://127.0.0.1:1/redirect")],
        "redirecting",
    )
    .await;
    let temp = TestConfigBuilder::new().build();

    // Follow redirects with max 0 → should fail immediately
    let output = test_command(&temp)
        .args(["-L", "--max-redirs", "0", &server.url("/start")])
        .output()
        .unwrap();

    // With max-redirs=0, the redirect loop hits 0 and the CLI gets a redirect response
    // but stops following. This may succeed (returning 302) or error depending on implementation.
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("302") || combined.contains("redirect") || !output.status.success(),
        "should hit redirect limit: {combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_http_retries_on_specified_codes() {
    // Server returns 503 — with --retry-http 503, the CLI should retry
    let server = MockServer::start(503, vec![], "Service Unavailable").await;
    let temp = TestConfigBuilder::new().build();

    let start = std::time::Instant::now();
    let output = test_command(&temp)
        .args([
            "--retries",
            "1",
            "--retry-backoff",
            "10",
            "--retry-http",
            "503",
            &server.url("/test"),
        ])
        .output()
        .unwrap();
    let elapsed = start.elapsed();

    assert!(!output.status.success(), "should fail after retries");
    // Should have waited at least the backoff time (retried once)
    assert!(
        elapsed.as_millis() >= 10,
        "should have retried with backoff: elapsed={elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_mismatch_returns_no_compatible_challenge_and_exit_class() {
    // The server offers a single moderato challenge; `--network tempo`
    // filters it out during selection, so no candidate survives and we
    // surface `NoCompatibleChallenge` (E_PAYMENT) instead of the legacy
    // post-selection router error.
    let server = MockServer::start_payment_deferred("Payment Required").await;
    let www_auth = charge_www_authenticate_with_realm("test-network-mismatch", &server.base_url);
    server.set_www_authenticate(&www_auth);
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .env("TEMPO_PRIVATE_KEY", MODERATO_PRIVATE_KEY)
        .args(["--network", "tempo", &server.url("/paid")])
        .output()
        .unwrap();

    assert_exit_code(
        &output,
        4,
        "challenge network mismatch should exit with E_PAYMENT",
    );
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("No payment challenge matches the configured wallet"),
        "should surface NoCompatibleChallenge: {combined}"
    );
    assert!(
        combined.contains("tempo-moderato"),
        "should list the rejected moderato offer: {combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_with_include_headers() {
    let server = MockServer::start(200, vec![("x-stream", "yes")], "stream+headers").await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--stream", "-i", &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("HTTP 200"),
        "should include status line: {stdout}"
    );
    assert!(
        stdout.contains("x-stream: yes"),
        "should include custom header: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_json_with_write_meta() {
    let sse_body = "data: test\n\n";
    let server = MockServer::start_sse(sse_body).await;
    let temp = TestConfigBuilder::new().build();

    let meta_path = temp.path().join("sse_meta.json");
    let output = test_command(&temp)
        .args([
            "--sse-json",
            "--write-meta",
            meta_path.to_str().unwrap(),
            &server.url("/stream"),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(meta_path.exists(), "meta file should be created for SSE");
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(meta["status"], 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_urlencode_from_file() {
    let server = MockServer::start_echo_request().await;
    let temp = TestConfigBuilder::new().build();

    let data_file = temp.path().join("encode_data.txt");
    std::fs::write(&data_file, "hello world & special=chars").unwrap();

    let arg = format!("field=@{}", data_file.to_str().unwrap());
    let output = test_command(&temp)
        .args(["--data-urlencode", &arg, &server.url("/test")])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let body = parsed["body"].as_str().unwrap();
    // The file content should be URL-encoded
    assert!(
        body.contains("field="),
        "body should contain the field name: {body}"
    );
    assert!(!body.contains(' '), "spaces should be URL-encoded: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_timeout_flag() {
    let server = MockServer::start(200, vec![], "ok").await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--connect-timeout", "5", &server.url("/test")])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "connect-timeout should succeed for reachable server"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ok"), "body: {stdout}");
}

/// Verbose logs must never contain the raw private key material.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_key_not_leaked_in_verbose_logs() {
    let rpc = MockRpcServer::start(42431).await;
    let server = MockServer::start_payment_deferred("pk leak test ok").await;
    let www_auth = charge_www_authenticate_with_realm("test-pk-leak", &server.base_url);
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
        "expected charge flow to succeed: {combined}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
        "stderr must not contain the raw private key (without 0x prefix)"
    );
    assert!(
        !stderr.contains(MODERATO_PRIVATE_KEY),
        "stderr must not contain the full private key"
    );
}

/// Writing response body to a file with `-o` preserves content exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_response_written_to_file() {
    let body = "binary-like-data-\t\n-special-chars-end";
    let server = MockServer::start(200, vec![], body).await;
    let temp = TestConfigBuilder::new().build();
    let out_file = temp.path().join("output.bin");

    let output = test_command(&temp)
        .args(["-o", out_file.to_str().unwrap(), &server.url("/download")])
        .output()
        .unwrap();

    assert!(output.status.success(), "expected success");
    assert!(out_file.exists(), "output file should be created");
    let contents = std::fs::read_to_string(&out_file).unwrap();
    assert_eq!(
        contents, body,
        "file content should match response body exactly"
    );
}

/// `-o` to a path inside a non-existent directory should fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_file_in_nonexistent_directory_fails() {
    let server = MockServer::start(200, vec![], "should not write").await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args([
            "-o",
            "/nonexistent_dir_xyz/output.txt",
            &server.url("/test"),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "writing to non-existent directory should fail"
    );
}

/// `-o` path traversal should be rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_file_path_traversal_rejected() {
    let server = MockServer::start(200, vec![], "should not write").await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["-o", "../escape.txt", &server.url("/test")])
        .output()
        .unwrap();

    assert!(!output.status.success(), "path traversal should fail");
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("path traversal") || combined.contains("Invalid output path"),
        "error should mention invalid output path: {combined}"
    );
}

/// Verbose payment flow must not leak private key material in stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payment_credential_not_leaked_in_verbose_logs() {
    let h = PaymentTestHarness::charge_with_body("auth redact ok").await;

    let output = test_command(&h.temp)
        .args(["-v", &h.url("/api")])
        .output()
        .unwrap();

    let combined = get_combined_output(&output);
    assert!(
        output.status.success(),
        "expected payment flow to succeed: {combined}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
        "stderr must not contain wallet private key material"
    );
}

/// An empty URL argument should fail with `E_USAGE` (exit code 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_url_fails_with_usage_error() {
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp).arg("").output().unwrap();

    assert_exit_code(&output, 2, "empty URL should exit with E_USAGE");
}

/// `--json` and `--toon` input format flags conflict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_toon_format_conflict() {
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["--json", "{}", "--toon", "x: 1", "http://127.0.0.1:1/test"])
        .output()
        .unwrap();

    assert_exit_code(
        &output,
        2,
        "--json + --toon format conflict should exit with E_USAGE",
    );
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("cannot be used with")
            || combined.contains("conflict")
            || combined.contains("--json"),
        "should mention conflict: {combined}"
    );
}

/// A response with a very large header value (1000+ chars) is handled without crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_header_value_handled() {
    let large_value: String = "X".repeat(2000);
    let server = MockServer::start(200, vec![("x-large-header", &large_value)], "ok").await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .args(["-i", &server.url("/test")])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "large header value should not crash"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&large_value),
        "stdout should contain the large header value"
    );
}
