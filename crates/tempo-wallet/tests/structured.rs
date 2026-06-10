//! Snapshot-like structure tests for JSON and TOON outputs.

use std::process::Output;

use serde_json::Value;

mod common;
use common::test_command;
use tempo_test::{
    assert_clean_stderr, assert_json_toon_equivalent, seed_local_session, MockServicesServer,
    TestConfigBuilder, MODERATO_DIRECT_KEYS_TOML,
};

fn run_both(temp: &tempfile::TempDir, args: &[&str]) -> (Output, Value, Output, Value) {
    tempo_test::run_structured_both(test_command, temp, args)
}

#[test]
fn sessions_list_json_and_toon_have_expected_shape() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build();
    seed_local_session(&temp, "https://example.com");

    let (json_out, json, toon_out, toon) = run_both(&temp, &["sessions", "list"]);
    assert_clean_stderr(&json_out);
    assert_clean_stderr(&toon_out);
    assert!(json.get("sessions").is_some());
    assert!(json.get("total").is_some());
    assert!(toon.get("sessions").is_some());
    assert!(toon.get("total").is_some());
}

#[test]
fn sessions_sync_empty_json_and_toon_shape() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build();
    let (json_out, json, toon_out, toon) = run_both(&temp, &["sessions", "sync"]);
    assert_clean_stderr(&json_out);
    assert_clean_stderr(&toon_out);
    assert!(json.get("sessions").is_some());
    assert!(json.get("total").is_some());
    assert!(toon.get("sessions").is_some());
    assert!(toon.get("total").is_some());
    assert_json_toon_equivalent(&json, &toon);
}

#[test]
fn sessions_sync_origin_missing_json_and_toon_shape() {
    let temp = TestConfigBuilder::new()
        .with_keys_toml(MODERATO_DIRECT_KEYS_TOML)
        .build();
    let (json_out, json, toon_out, toon) = run_both(
        &temp,
        &["sessions", "sync", "--origin", "https://example.invalid"],
    );
    assert_clean_stderr(&json_out);
    assert_clean_stderr(&toon_out);
    assert!(json.get("sessions").is_some());
    assert!(json.get("total").is_some());
    assert!(toon.get("sessions").is_some());
    assert!(toon.get("total").is_some());
    assert_json_toon_equivalent(&json, &toon);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn services_json_and_toon_shapes() {
    let mock = MockServicesServer::start().await;

    let temp = TestConfigBuilder::new().build();

    let mut cmd = test_command(&temp);
    let out_json = cmd
        .env("TEMPO_SERVICES_URL", &mock.services_url)
        .args(["-j", "services", "list"])
        .output()
        .unwrap();
    assert!(out_json.status.success());
    assert_clean_stderr(&out_json);
    let json_list: Value = serde_json::from_str(String::from_utf8_lossy(&out_json.stdout).trim())
        .expect("valid services list json");
    assert!(json_list.as_array().is_some_and(|a| !a.is_empty()));

    let mut cmd = test_command(&temp);
    let out_toon = cmd
        .env("TEMPO_SERVICES_URL", &mock.services_url)
        .args(["-t", "services", "list"])
        .output()
        .unwrap();
    assert!(out_toon.status.success());
    assert_clean_stderr(&out_toon);
    let toon_list: Value =
        toon_format::decode_default(String::from_utf8_lossy(&out_toon.stdout).trim())
            .expect("valid services list toon");
    assert!(toon_list.as_array().is_some_and(|a| !a.is_empty()));

    let mut cmd = test_command(&temp);
    let out_json = cmd
        .env("TEMPO_SERVICES_URL", &mock.services_url)
        .args(["-j", "services", "openai"])
        .output()
        .unwrap();
    assert!(out_json.status.success());
    assert_clean_stderr(&out_json);
    let json_info: Value = serde_json::from_str(String::from_utf8_lossy(&out_json.stdout).trim())
        .expect("valid services info json");
    assert_eq!(json_info["id"], "openai");
    assert_eq!(json_info["supportsCredits"], true);

    let mut cmd = test_command(&temp);
    let out_toon = cmd
        .env("TEMPO_SERVICES_URL", &mock.services_url)
        .args(["-t", "services", "openai"])
        .output()
        .unwrap();
    assert!(out_toon.status.success());
    assert_clean_stderr(&out_toon);
    let toon_info: Value =
        toon_format::decode_default(String::from_utf8_lossy(&out_toon.stdout).trim())
            .expect("valid services info toon");
    assert_eq!(toon_info["id"], "openai");
    assert_eq!(toon_info["supportsCredits"], true);
}

#[test]
fn whoami_json_and_toon_shapes() {
    let temp = TestConfigBuilder::new().build();
    let (json_out, json, toon_out, toon) = run_both(&temp, &["whoami"]);
    assert_clean_stderr(&json_out);
    assert_clean_stderr(&toon_out);
    assert_eq!(json["ready"], false);
    assert_eq!(toon["ready"], false);
    assert_json_toon_equivalent(&json, &toon);
}

#[test]
fn logout_json_and_toon_shapes() {
    let temp = TestConfigBuilder::new().build();
    let (json_out, json, toon_out, toon) = run_both(&temp, &["logout", "--yes"]);
    assert_clean_stderr(&json_out);
    assert_clean_stderr(&toon_out);
    assert_eq!(json["logged_in"], false);
    assert_eq!(json["disconnected"], false);
    assert_eq!(toon["logged_in"], false);
    assert_eq!(toon["disconnected"], false);
    assert_json_toon_equivalent(&json, &toon);
}

#[test]
fn keys_empty_json_and_toon_shapes() {
    let temp = TestConfigBuilder::new().build();
    let (json_out, json, toon_out, toon) = run_both(&temp, &["keys"]);
    assert_clean_stderr(&json_out);
    assert_clean_stderr(&toon_out);
    assert!(json["keys"].as_array().is_some());
    assert_eq!(json["total"], 0);
    assert!(toon["keys"].as_array().is_some());
    assert_eq!(toon["total"], 0);
    assert_json_toon_equivalent(&json, &toon);
}

#[test]
fn version_json_output() {
    let temp = TestConfigBuilder::new().build();
    let out = test_command(&temp)
        .args(["-j", "--version"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_clean_stderr(&out);
    let parsed: Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).expect("valid json");
    assert!(parsed["version"].is_string());
    assert!(parsed["git_commit"].is_string());
    assert!(parsed["build_date"].is_string());
    assert!(parsed["profile"].is_string());
}

#[test]
fn version_toon_output() {
    let temp = TestConfigBuilder::new().build();
    let out = test_command(&temp)
        .args(["-t", "--version"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_clean_stderr(&out);
    let parsed: Value = toon_format::decode_default(String::from_utf8_lossy(&out.stdout).trim())
        .expect("valid toon");
    assert!(parsed["version"].is_string());
    assert!(parsed["git_commit"].is_string());
    assert!(parsed["build_date"].is_string());
    assert!(parsed["profile"].is_string());
}

#[test]
fn describe_outputs_schema() {
    let temp = TestConfigBuilder::new().build();
    let out = test_command(&temp).arg("--describe").output().unwrap();
    assert!(out.status.success());
    assert_clean_stderr(&out);
    let parsed: Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).expect("valid json");
    assert!(parsed["name"].is_string());
    assert!(parsed.get("subcommands").is_some());
}
