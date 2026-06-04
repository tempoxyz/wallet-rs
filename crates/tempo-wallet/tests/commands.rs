//! Integration tests for tempo-wallet commands.

mod common;
mod session;

use common::test_command;
use tempo_test::{
    assert_exit_code, get_combined_output, MockServer, MockServicesServer, TestConfigBuilder,
    MODERATO_DIRECT_KEYS_TOML,
};

fn parse_events_log(path: &std::path::Path) -> Vec<(String, serde_json::Value)> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .filter_map(|line| {
            let (name, json_str) = line.split_once('|')?;
            let value: serde_json::Value = serde_json::from_str(json_str).ok()?;
            Some((name.to_string(), value))
        })
        .collect()
}
// ==================== whoami ====================

#[test]
fn whoami_no_wallet_shows_not_logged_in() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp).arg("whoami").output().unwrap();

    assert!(
        output.status.success(),
        "whoami should succeed even without wallet"
    );
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("Not logged in") || combined.contains("not logged in"),
        "should mention not logged in: {combined}"
    );
}

#[test]
fn whoami_no_wallet_json_shape() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp).args(["-j", "whoami"]).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["ready"], false, "should not be ready: {parsed}");
}

#[test]
fn whoami_with_wallet_json_has_wallet_field() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build();

    let output = test_command(&temp).args(["-j", "whoami"]).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(
        parsed["wallet"].is_string(),
        "should have wallet field: {parsed}"
    );
    assert!(
        parsed["wallet"].as_str().unwrap().starts_with("0x"),
        "wallet should be an address: {parsed}"
    );
}

#[test]
fn whoami_with_wallet_toon_shape() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build();

    let output = test_command(&temp).args(["-t", "whoami"]).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = toon_format::decode_default(stdout.trim()).unwrap();
    assert!(
        parsed["wallet"].is_string(),
        "TOON should have wallet: {parsed}"
    );
}

#[test]
fn whoami_emits_keystore_degraded_event_for_malformed_keys_file() {
    let temp = TestConfigBuilder::new().build();
    let keys_path = temp.path().join(".tempo/wallet/keys.toml");
    std::fs::write(&keys_path, "this-is-not-valid-toml = [").unwrap();

    let events_path = temp.path().join("events_keystore_degraded.log");
    let output = test_command(&temp)
        .env("TEMPO_TEST_EVENTS", events_path.to_str().unwrap())
        .arg("whoami")
        .output()
        .unwrap();

    assert!(output.status.success());
    let events = parse_events_log(&events_path);
    let payload = events
        .iter()
        .find(|(name, _)| name == "keystore load degraded")
        .map_or_else(
            || panic!("missing keystore load degraded event: {events:?}"),
            |(_, payload)| payload,
        );

    assert!(
        payload["strict_parse_failures"].as_u64().unwrap_or(0) >= 1,
        "expected strict_parse_failures >= 1, got: {payload}"
    );
}

// ==================== logout ====================

#[test]
fn logout_no_wallet_succeeds() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp)
        .args(["logout", "--yes"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("Not logged in") || combined.contains("not logged in"),
        "should mention not logged in: {combined}"
    );
}

#[test]
fn logout_no_wallet_json_shape() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp)
        .args(["-j", "logout", "--yes"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["logged_in"], false);
    assert_eq!(parsed["disconnected"], false);
}

#[test]
fn logout_without_yes_in_non_interactive_mode_requires_confirmation_flag() {
    use std::process::Stdio;

    let passkey_keys = r#"
[[keys]]
wallet_type = "passkey"
wallet_address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
key_address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
chain_id = 42431
"#;
    let temp = TestConfigBuilder::new()
        .with_keys_toml(passkey_keys)
        .build();
    let mut child = test_command(&temp)
        .arg("logout")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn logout command");

    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .expect("failed to wait for logout command");

    assert_exit_code(
        &output,
        2,
        "non-interactive logout without --yes should exit with E_USAGE",
    );
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("Use --yes for non-interactive mode"),
        "expected non-interactive confirmation guidance: {combined}"
    );
}

// ==================== keys ====================

#[test]
fn keys_empty() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp).arg("keys").output().unwrap();

    assert!(output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("No keys") || combined.contains("0 key"),
        "should mention no keys: {combined}"
    );
}

#[test]
fn keys_json_shape() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build();

    let output = test_command(&temp).args(["-j", "keys"]).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed["keys"].is_array());
    assert!(parsed["total"].as_u64().unwrap() >= 1);
    let key = &parsed["keys"][0];
    assert!(key["address"].is_string());
    assert!(key["key"].is_string(), "JSON should include private key");
}

#[test]
fn keys_toon_shape() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build();

    let output = test_command(&temp).args(["-t", "keys"]).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = toon_format::decode_default(stdout.trim()).unwrap();
    assert!(parsed["keys"].is_array());
    assert!(parsed["total"].as_u64().unwrap() >= 1);
}

#[test]
fn mixed_case_keys_are_canonicalized_in_output() {
    let mixed_case_keys = r#"
[[keys]]
wallet_address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
key_address = "0xF39fD6E51Aad88f6f4ce6AB8827279cfFFb92266"
key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
chain_id = 42431
"#;
    let temp = TestConfigBuilder::new()
        .with_keys_toml(mixed_case_keys)
        .build();

    let whoami = test_command(&temp).args(["-j", "whoami"]).output().unwrap();
    assert!(whoami.status.success());
    let whoami_json: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&whoami.stdout).trim()).unwrap();
    assert_eq!(
        whoami_json["wallet"].as_str(),
        Some("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266")
    );

    let keys = test_command(&temp).args(["-j", "keys"]).output().unwrap();
    assert!(keys.status.success());
    let keys_json: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&keys.stdout).trim()).unwrap();
    assert_eq!(
        keys_json["keys"][0]["wallet_address"].as_str(),
        Some("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266")
    );
    assert_eq!(
        keys_json["keys"][0]["address"].as_str(),
        Some("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266")
    );
}

// ==================== services ====================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn services_category_filter() {
    let mock = MockServicesServer::start().await;
    let temp = TestConfigBuilder::new().build();

    // Filter by existing category
    let output = test_command(&temp)
        .env("TEMPO_SERVICES_URL", &mock.services_url)
        .args(["-j", "services", "--search", "ai"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed.as_array().is_some_and(|a| !a.is_empty()));

    // Filter by non-existent category
    let output = test_command(&temp)
        .env("TEMPO_SERVICES_URL", &mock.services_url)
        .args(["-j", "services", "--search", "nonexistent"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed.as_array().is_some_and(std::vec::Vec::is_empty));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn services_search_filter() {
    let mock = MockServicesServer::start().await;
    let temp = TestConfigBuilder::new().build();

    // Search for "openai" (matches the mock service)
    let output = test_command(&temp)
        .env("TEMPO_SERVICES_URL", &mock.services_url)
        .args(["-j", "services", "--search", "openai"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed.as_array().is_some_and(|a| !a.is_empty()));

    // Search for something that doesn't match
    let output = test_command(&temp)
        .env("TEMPO_SERVICES_URL", &mock.services_url)
        .args(["-j", "services", "--search", "zzz_no_match"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed.as_array().is_some_and(std::vec::Vec::is_empty));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn services_info_not_found() {
    let mock = MockServicesServer::start().await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .env("TEMPO_SERVICES_URL", &mock.services_url)
        .args(["services", "nonexistent_service"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("not found"),
        "should mention not found: {combined}"
    );
}

#[test]
fn services_invalid_url_is_classified_as_usage_error() {
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .env("TEMPO_SERVICES_URL", "not-a-valid-url")
        .args(["services", "list"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_exit_code(
        &output,
        2,
        "invalid TEMPO_SERVICES_URL should map to InvalidUsage",
    );

    let combined = get_combined_output(&output);
    assert!(
        combined.contains("invalid service directory URL"),
        "should report invalid service URL: {combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn services_http_error_includes_response_body() {
    let mock = MockServer::start(503, vec![], "maintenance window").await;
    let temp = TestConfigBuilder::new().build();

    let output = test_command(&temp)
        .env("TEMPO_SERVICES_URL", format!("{}/services", mock.base_url))
        .args(["services", "list"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("HTTP 503"),
        "should report the HTTP status: {combined}"
    );
    assert!(
        combined.contains("maintenance window"),
        "should include the service directory response body: {combined}"
    );
}

// ==================== version ====================

#[test]
fn version_flag_outputs_version() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp).arg("--version").output().unwrap();

    assert!(output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("tempo wallet"),
        "should show version: {combined}"
    );
}

#[test]
fn help_lists_refresh_command() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp).arg("--help").output().unwrap();

    assert!(output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("refresh"),
        "help should list refresh command: {combined}"
    );
}

#[test]
fn refresh_rejects_non_passkey_wallets_with_guidance() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build();

    let output = test_command(&temp)
        .args(["-n", "tempo-moderato", "refresh"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("only supported for passkey wallets"),
        "should explain refresh support boundary: {combined}"
    );
    assert!(
        combined.contains("tempo wallet login"),
        "should include recovery command: {combined}"
    );
}

#[test]
fn refresh_failed_login_restores_existing_passkey_key() {
    let passkey_keys = r#"
[[keys]]
wallet_type = "passkey"
wallet_address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
key_address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
chain_id = 42431
"#;
    let temp = TestConfigBuilder::new()
        .with_keys_toml(passkey_keys)
        .build();
    let keys_path = temp.path().join(".tempo/wallet/keys.toml");

    let output = test_command(&temp)
        .env("TEMPO_AUTH_URL", "not-a-valid-url")
        .args(["-n", "tempo-moderato", "refresh"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("Restored previous access key"),
        "should report rollback after failed refresh: {combined}"
    );

    let keys_body = std::fs::read_to_string(&keys_path).unwrap();
    assert!(
        keys_body.contains("[[keys]]"),
        "refresh failure must preserve key entries: {keys_body}"
    );
    assert!(
        keys_body.contains("wallet_type = \"passkey\""),
        "preserved key should remain passkey"
    );
}

// ==================== transfer ====================

#[test]
fn transfer_help_shows_flags() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp)
        .args(["transfer", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("<TO>") || combined.contains("[TO]"),
        "should show TO arg: {combined}"
    );
    assert!(combined.contains("--dry-run"), "should show --dry-run flag");
    assert!(
        combined.contains("--fee-token"),
        "should show --fee-token flag"
    );
    assert!(
        combined.contains("Token contract address"),
        "should describe token as contract address: {combined}"
    );
}

#[test]
fn transfer_no_wallet_fails() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp)
        .args([
            "transfer",
            "1.00",
            "0x20c0000000000000000000000b9537d11c60e8b50",
            "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("No wallet") || combined.contains("login"),
        "should mention no wallet or login: {combined}"
    );
}

#[test]
fn transfer_no_wallet_json_fails() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp)
        .args([
            "-j",
            "transfer",
            "1.00",
            "0x20c0000000000000000000000b9537d11c60e8b50",
            "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
}

#[test]
fn transfer_missing_recipient_fails() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp)
        .args([
            "transfer",
            "1.00",
            "0x20c0000000000000000000000b9537d11c60e8b50",
        ])
        .output()
        .unwrap();

    assert_exit_code(&output, 2, "missing recipient should exit with E_USAGE");
}

#[test]
fn transfer_missing_amount_fails() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp).args(["transfer"]).output().unwrap();

    assert_exit_code(&output, 2, "missing amount should exit with E_USAGE");
}

#[test]
fn transfer_missing_token_fails() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp)
        .args(["transfer", "1.00"])
        .output()
        .unwrap();

    assert_exit_code(&output, 2, "missing token should exit with E_USAGE");
}

#[test]
fn transfer_invalid_token_address_fails() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build();

    let output = test_command(&temp)
        .args([
            "-n",
            "tempo-moderato",
            "transfer",
            "1.00",
            "not-an-address",
            "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("token address"),
        "should mention token address error: {combined}"
    );
}

#[test]
fn transfer_invalid_recipient_address_fails() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build();

    // Use a non-0x string for recipient to trigger the validate_hex_input error
    // before any on-chain calls happen
    let output = test_command(&temp)
        .args([
            "-n",
            "tempo-moderato",
            "transfer",
            "1.00",
            "0x0000000000000000000000000000000000000001",
            "not-an-address",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let combined = get_combined_output(&output);
    assert!(
        combined.contains("recipient address"),
        "should mention recipient address error: {combined}"
    );
}

// ==================== unknown subcommand ====================

#[test]
fn unknown_subcommand_fails() {
    let temp = TestConfigBuilder::new().build();
    let output = test_command(&temp).arg("nonexistent").output().unwrap();

    assert_exit_code(&output, 2, "unknown subcommand should exit with E_USAGE");
}
