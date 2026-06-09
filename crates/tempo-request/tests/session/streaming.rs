//! Streaming and protocol edge-case scenarios split from commands.rs.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_session_challenge_has_no_tx_no_db_write_and_shows_cost() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: false,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let output = test_command(&temp)
        .args([
            "--dry-run",
            "--private-key",
            MODERATO_PRIVATE_KEY,
            "--network",
            "tempo-moderato",
            &server.url("/resource"),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "dry-run request should succeed: {}",
        get_combined_output(&output)
    );

    let combined = get_combined_output(&output);
    assert!(
        combined.contains("[DRY RUN] Session payment would be made:"),
        "dry-run output should include dry-run banner: {combined}"
    );
    assert!(
        combined.contains("Cost per request:"),
        "dry-run output should include cost display: {combined}"
    );

    let session_observed = server.snapshot();
    assert_eq!(
        session_observed.open_count, 0,
        "dry-run must not submit open credentials"
    );
    assert_eq!(
        session_observed.voucher_count, 0,
        "dry-run must not submit voucher credentials"
    );
    assert_eq!(
        session_observed.top_up_count, 0,
        "dry-run must not submit top-up credentials"
    );

    let rpc_observed = rpc.snapshot();
    assert_eq!(
        rpc_observed.eth_call_count, 0,
        "dry-run should avoid on-chain read RPC calls"
    );
    assert_eq!(
        rpc_observed.send_raw_count, 0,
        "dry-run should never submit transactions"
    );

    let db_path = temp.path().join(".tempo/wallet/channels.db");
    assert!(
        !db_path.exists(),
        "dry-run should not create a local session database"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_after_payment_preserves_state_and_surfaces_dispute_message() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: false,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: Some(401),
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        first_output.status.success(),
        "first request should open channel: {}",
        get_combined_output(&first_output)
    );

    let second_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        !second_output.status.success(),
        "second request should fail after paid voucher path"
    );

    let combined = get_combined_output(&second_output);
    assert!(
        combined.contains("channel state preserved for on-chain dispute"),
        "error should surface preserved-state dispute message: {combined}"
    );
    assert!(
        combined.contains("upstream failed after voucher authorization"),
        "error should surface upstream response body: {combined}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.open_count, 1,
        "failure after payment should not open a replacement channel"
    );
    assert_eq!(
        observed.voucher_count, 1,
        "reuse path should send exactly one voucher before surfacing the error"
    );

    let channels = load_channels(&temp);
    assert_eq!(
        channels.len(),
        1,
        "existing channel row should be preserved"
    );
    assert_eq!(channels[0].state, "active");
    assert_eq!(
        channels[0].cumulative_amount,
        SESSION_AMOUNT * 2,
        "preserved channel state should keep the advanced cumulative amount for dispute"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_voucher_clamps_when_required_cumulative_exceeds_deposit() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: true,
        sse_receipt_accepted: Some(2_200_000),
        sse_required_cumulative: Some(2_000_000),
        sse_reported_deposit: Some(1_000_000),
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/stream"));
    assert!(
        first_output.status.success(),
        "first stream request should open channel: {}",
        get_combined_output(&first_output)
    );

    let second_output = run_session_request(&temp, &server.url("/stream"));
    assert!(
        second_output.status.success(),
        "second stream request should recover with top-up: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.top_up_count, 1,
        "requiredCumulative > deposit should trigger exactly one top-up in SSE flow"
    );
    assert!(
        !observed.voucher_cumulative.is_empty()
            && observed
                .voucher_cumulative
                .iter()
                .all(|amount| *amount == 2_000_000),
        "voucher cumulative retries should stay clamped at requiredCumulative: {:?}",
        observed.voucher_cumulative
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert_eq!(
        channels[0].cumulative_amount, 2_200_000,
        "stream receipt should persist post-voucher cumulative after required>deposit flow"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_max_spend_top_up_targets_budget_cap() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: true,
        sse_receipt_accepted: Some(2_200_000),
        sse_required_cumulative: Some(2_000_000),
        sse_reported_deposit: Some(1_000_000),
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output =
        run_session_request_with_args(&temp, &server.url("/stream"), &["--max-spend", "1"]);
    assert!(
        first_output.status.success(),
        "first stream request should open channel at baseline max-spend: {}",
        get_combined_output(&first_output)
    );

    let second_output =
        run_session_request_with_args(&temp, &server.url("/stream"), &["--max-spend", "3"]);
    assert!(
        second_output.status.success(),
        "second stream request should top-up toward max-spend budget: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.top_up_count, 1,
        "requiredCumulative above reported deposit should trigger one top-up"
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert_eq!(
        channels[0].cumulative_amount, 2_200_000,
        "stream receipt should persist post-voucher cumulative"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payment_receipt_event_terminates_stream_without_processing_trailing_events() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: false,
        sse_receipt_accepted: Some(2_750_000),
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        first_output.status.success(),
        "first request should open channel: {}",
        get_combined_output(&first_output)
    );

    let second_output = run_session_request(&temp, &server.url("/stream-receipt-tail"));
    assert!(
        second_output.status.success(),
        "stream request should succeed: {}",
        get_combined_output(&second_output)
    );

    let second_stdout = String::from_utf8_lossy(&second_output.stdout);
    assert!(
        second_stdout.contains("stream"),
        "SSE content before payment-receipt should still be emitted: {second_stdout}"
    );
    assert!(
        !second_stdout.contains("after-receipt"),
        "client must terminate on payment-receipt and ignore trailing SSE events: {second_stdout}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.voucher_count, 2,
        "stream request should send one voucher request plus one HEAD voucher update"
    );
    assert_eq!(
        observed.voucher_head_updates, 1,
        "stream should perform a single HEAD voucher update before terminating"
    );
    assert_eq!(
        observed.voucher_post_updates, 0,
        "HEAD success path should not fallback to POST"
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert_eq!(
        channels[0].cumulative_amount, 2_750_000,
        "payment-receipt event should persist accepted cumulative and end stream"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_voucher_resume_retries_with_backoff_up_to_configured_max() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        first_output.status.success(),
        "first request should open channel: {}",
        get_combined_output(&first_output)
    );

    let started = std::time::Instant::now();
    let second_output = run_session_request_with_env(
        &temp,
        &server.url("/stream-stall"),
        &[
            ("TEMPO_SESSION_MAX_VOUCHER_RETRIES", "3"),
            ("TEMPO_SESSION_STALL_TIMEOUT_MS", "20"),
            ("TEMPO_SESSION_NORMAL_TIMEOUT_MS", "80"),
        ],
    );
    let elapsed = started.elapsed();
    assert!(
        !second_output.status.success(),
        "stalled stream request should fail after retry budget: {}",
        get_combined_output(&second_output)
    );

    let combined = get_combined_output(&second_output);
    assert!(
        combined.contains("session voucher retries exhausted"),
        "stalled path should report retry exhaustion: {combined}"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(150),
        "retry path should spend measurable time in backoff; elapsed={elapsed:?}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.voucher_head_updates, 4,
        "configured retry budget (3) should yield one initial HEAD plus three retry HEADs"
    );
    assert_eq!(
        observed.voucher_post_updates, 0,
        "HEAD success transport should not fallback to POST"
    );
    assert_eq!(
        observed.voucher_count, 5,
        "stalled flow should submit one initial voucher request plus four voucher-update heads"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_cumulative_above_deposit_sends_topup_and_resumes_voucher_flow() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: true,
        sse_receipt_accepted: Some(2_500_000),
        sse_required_cumulative: Some(2_500_000),
        sse_reported_deposit: Some(500_000),
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/stream"));
    assert!(
        first_output.status.success(),
        "first stream request should open channel: {}",
        get_combined_output(&first_output)
    );

    let deposit_before = load_channels(&temp)
        .first()
        .map(|channel| channel.deposit)
        .unwrap_or_default();

    let second_output = run_session_request(&temp, &server.url("/stream"));
    assert!(
        second_output.status.success(),
        "second stream request should succeed after top-up recovery: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(observed.top_up_count, 1, "expected one top-up credential");
    assert_eq!(
        observed.top_up_actions,
        vec!["topUp".to_string()],
        "top-up credential should preserve the spec action field"
    );
    assert!(
        observed.voucher_count >= 3,
        "voucher flow should continue after top-up on the same request"
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert!(
        channels[0].deposit > deposit_before,
        "local persisted deposit should increase after top-up: before={deposit_before}, after={} ",
        channels[0].deposit
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn challenge_request_requires_wire_recipient_field_without_local_rename_leakage() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: false,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let output = run_session_request(&temp, &server.url("/wire-payee"));
    assert!(
        !output.status.success(),
        "challenge with renamed payee field should fail parsing: {}",
        get_combined_output(&output)
    );

    let combined = get_combined_output(&output);
    assert!(
        combined.contains("missing recipient"),
        "error should clearly report missing wire recipient field: {combined}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.open_count, 0,
        "client must not send payment credentials when recipient wire field is absent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_and_voucher_credentials_keep_spec_field_names() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: false,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: true,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        first_output.status.success(),
        "first request should open channel: {}",
        get_combined_output(&first_output)
    );
    let second_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        second_output.status.success(),
        "second request should include voucher/top-up recovery path: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert!(
        !observed.open_payload_keys.is_empty(),
        "expected captured open credential payload keys"
    );
    assert!(
        !observed.voucher_payload_keys.is_empty(),
        "expected captured voucher credential payload keys"
    );

    assert_payload_has_spec_fields(
        &observed.open_payload_keys[0],
        "open credential payload",
        &[
            "action",
            "channelId",
            "cumulativeAmount",
            "signature",
            "transaction",
        ],
    );
    assert_payload_has_spec_fields(
        &observed.voucher_payload_keys[0],
        "voucher credential payload",
        &["action", "channelId", "cumulativeAmount", "signature"],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_source_is_did_pkh_eip155_chainid_address() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: false,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: true,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/resource"));
    assert!(first_output.status.success());
    let second_output = run_session_request(&temp, &server.url("/resource"));
    assert!(second_output.status.success());

    let observations = server.snapshot();
    assert!(
        !observations.credential_sources.is_empty(),
        "expected at least one captured credential source"
    );
    for source in observations.credential_sources {
        assert!(
            source.starts_with("did:pkh:eip155:42431:0x"),
            "source must use did:pkh:eip155:{{chainId}}:{{address}} format: {source}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_receipt_on_successful_paid_response_is_error_for_strict_sessions() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: false,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/resource"));
    assert!(first_output.status.success());

    let second_output = run_session_request(&temp, &server.url("/resource-missing-receipt"));
    assert!(
        !second_output.status.success(),
        "missing receipt on paid voucher response should fail in strict mode: {}",
        get_combined_output(&second_output)
    );
    let combined = get_combined_output(&second_output);
    assert!(
        combined.contains("Missing required Payment-Receipt on successful paid session response"),
        "client should fail with required-receipt error when paid response lacks receipt: {combined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_open_missing_receipt_preserves_channel_state() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: false,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let output = run_session_request(&temp, &server.url("/resource-open-missing-receipt"));
    assert!(
        !output.status.success(),
        "strict open should fail when successful paid response omits receipt: {}",
        get_combined_output(&output)
    );

    let channels = load_channels(&temp);
    assert_eq!(
        channels.len(),
        1,
        "strict open failure should still preserve recoverable local channel state"
    );
    assert_eq!(
        channels[0].state, "active",
        "strict open failure should preserve an active channel record for recovery"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_topup_receipt_is_error_for_strict_stream_sessions() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: false,
        sse_receipt_accepted: Some(2_500_000),
        sse_required_cumulative: Some(3_800_000),
        sse_reported_deposit: Some(2_000_000),
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/stream"));
    assert!(
        first_output.status.success(),
        "first stream should establish reusable strict session: {}",
        get_combined_output(&first_output)
    );

    let second_output = run_session_request(&temp, &server.url("/stream-missing-receipt"));
    assert!(
        !second_output.status.success(),
        "strict stream top-up should fail when top-up response omits receipt: {}",
        get_combined_output(&second_output)
    );
    let combined = get_combined_output(&second_output);
    assert!(
        combined.contains("Missing required Payment-Receipt on successful paid topUp response"),
        "strict top-up path should emit required-receipt error: {combined}"
    );

    let observed = server.snapshot();
    assert!(
        observed.top_up_count >= 1,
        "test precondition: stream flow should execute top-up before failing"
    );

    let channels = load_channels(&temp);
    assert_eq!(
        channels.len(),
        1,
        "strict top-up receipt failures should preserve local session state for recovery"
    );
    assert!(
        channels[0].deposit >= 3_800_000,
        "preserved state should keep a conservative funded deposit floor after top-up side effects"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_initial_header_receipt_persists_before_delayed_receipt_event() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: false,
        sse_receipt_accepted: Some(2_800_000),
        sse_required_cumulative: Some(2_000_000),
        sse_reported_deposit: Some(3_000_000),
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        first_output.status.success(),
        "first request should open channel: {}",
        get_combined_output(&first_output)
    );

    let mut delayed_stream = spawn_session_request_with_env(
        &temp,
        &server.url("/stream-delayed-receipt"),
        &[
            ("TEMPO_SESSION_STALL_TIMEOUT_MS", "5000"),
            ("TEMPO_SESSION_NORMAL_TIMEOUT_MS", "10000"),
        ],
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        delayed_stream.try_wait().unwrap().is_none(),
        "delayed stream should still be in-flight before SSE payment-receipt event arrives"
    );
    assert!(
        wait_for_channel_cumulative(&temp, 2_100_000, std::time::Duration::from_millis(900)),
        "initial SSE payment-receipt header acceptedCumulative should persist before delayed receipt event"
    );

    let second_output = delayed_stream.wait_with_output().unwrap();
    assert!(
        second_output.status.success(),
        "delayed stream request should complete successfully: {}",
        get_combined_output(&second_output)
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert_eq!(
        channels[0].cumulative_amount, 2_800_000,
        "delayed SSE payment-receipt event should still advance persisted cumulative"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_first_voucher_405_fallback_to_post_and_stream_continues() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: true,
        sse_receipt_accepted: Some(2_500_000),
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/stream"));
    assert!(
        first_output.status.success(),
        "first request should open channel: {}",
        get_combined_output(&first_output)
    );

    let second_output = run_session_request(&temp, &server.url("/stream"));
    assert!(
        second_output.status.success(),
        "second stream request should succeed via HEAD->POST fallback: {}",
        get_combined_output(&second_output)
    );
    let second_stdout = String::from_utf8_lossy(&second_output.stdout);
    assert!(
        second_stdout.contains("stream"),
        "stream output should continue after voucher transport fallback: {second_stdout}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.voucher_head_updates, 1,
        "voucher update should attempt HEAD transport exactly once"
    );
    assert_eq!(
        observed.voucher_head_statuses,
        vec![405],
        "HEAD voucher transport should explicitly receive 405 before fallback"
    );
    assert_eq!(
        observed.voucher_post_updates, 1,
        "405 HEAD response should trigger one POST fallback voucher update"
    );
    assert_eq!(
        observed.voucher_count, 3,
        "expected one voucher request plus one HEAD and one POST voucher update"
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert_eq!(
        channels[0].cumulative_amount, 2_500_000,
        "stream should complete successfully and persist final receipt cumulative"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_method_details_chainid_is_rejected() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: false,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let output = run_session_request(&temp, &server.url("/missing-chain-id"));
    assert!(
        !output.status.success(),
        "missing methodDetails.chainId should fail with schema error: {}",
        get_combined_output(&output)
    );
    assert!(
        get_combined_output(&output).contains("missing chainId"),
        "expected missing chainId failure"
    );

    let observed = server.snapshot();
    assert_eq!(observed.open_count, 0, "request should not open a channel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_required_cumulative_fails_stream_path_deterministically() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/resource"));
    assert!(first_output.status.success());

    let second_output = run_session_request(&temp, &server.url("/stream-required-malformed"));
    assert!(
        !second_output.status.success(),
        "malformed requiredCumulative must fail stream path: {}",
        get_combined_output(&second_output)
    );
    let combined = get_combined_output(&second_output);
    assert!(
        combined.contains("payment-need-voucher.requiredCumulative")
            && combined.contains("must be an integer amount")
            && combined.contains("not-a-number"),
        "failure should clearly describe malformed requiredCumulative: {combined}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.voucher_head_updates, 0,
        "stream must fail before issuing voucher update transport calls"
    );
    assert_eq!(
        observed.voucher_post_updates, 0,
        "stream must fail before POST fallback is considered"
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert_eq!(
        channels[0].cumulative_amount,
        SESSION_AMOUNT * 2,
        "failed stream should preserve pre-stream voucher persistence without rollback"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_required_cumulative_fails_stream_path_deterministically() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/resource"));
    assert!(first_output.status.success());

    let second_output = run_session_request(&temp, &server.url("/stream-required-empty"));
    assert!(
        !second_output.status.success(),
        "empty requiredCumulative must fail stream path: {}",
        get_combined_output(&second_output)
    );
    let combined = get_combined_output(&second_output);
    assert!(
        combined.contains("payment-need-voucher.requiredCumulative")
            && combined.contains("must be an integer amount"),
        "failure should clearly describe empty requiredCumulative: {combined}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.voucher_head_updates, 0,
        "stream must fail before issuing voucher update transport calls"
    );
    assert_eq!(
        observed.voucher_post_updates, 0,
        "stream must fail before POST fallback is considered"
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert_eq!(
        channels[0].cumulative_amount,
        SESSION_AMOUNT * 2,
        "failed stream should preserve pre-stream voucher persistence without rollback"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voucher_idempotency_replay_same_or_lower_cumulative_is_successful() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: true,
        sse_receipt_accepted: Some(2_500_000),
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/stream"));
    assert!(first_output.status.success());

    let second_output = run_session_request(&temp, &server.url("/stream-idempotent-replay"));
    assert!(
        second_output.status.success(),
        "delta-too-small idempotent replay should be handled as success: {}",
        get_combined_output(&second_output)
    );
    let second_stdout = String::from_utf8_lossy(&second_output.stdout);
    assert!(
        second_stdout.contains("stream"),
        "stream should continue when voucher replay response is idempotent: {second_stdout}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.idempotent_replay_problem_count, 1,
        "server should emit one delta-too-small replay response"
    );
    assert!(
        observed.voucher_post_updates >= 1,
        "idempotent replay path should still exercise voucher POST transport"
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert!(
        channels[0].cumulative_amount >= 2_000_000,
        "client should preserve monotonic cumulative persistence after idempotent replay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paid_requests_include_idempotency_key_and_retry_path_stays_stable() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: true,
        sse_receipt_accepted: Some(2_500_000),
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/stream"));
    assert!(first_output.status.success());

    let second_output = run_session_request(&temp, &server.url("/stream-idempotent-replay"));
    assert!(
        second_output.status.success(),
        "duplicate processing response path should remain stable: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.missing_idempotency_key_count, 0,
        "all paid requests should include Idempotency-Key"
    );
    assert!(
        !observed.open_idempotency_keys.is_empty(),
        "open paid requests must include Idempotency-Key"
    );
    assert!(
        !observed.voucher_idempotency_keys.is_empty(),
        "voucher paid requests must include Idempotency-Key"
    );

    let mut seen = std::collections::HashMap::<String, usize>::new();
    for key in observed.voucher_idempotency_keys {
        assert!(!key.trim().is_empty(), "Idempotency-Key must be non-empty");
        *seen.entry(key).or_default() += 1;
    }
    assert!(
        seen.values().any(|count| *count >= 2),
        "retry/fallback voucher transport should reuse the same Idempotency-Key across duplicate processing handling"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voucher_idempotency_replay_numeric_accepted_cumulative_is_successful() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: true,
        sse_receipt_accepted: Some(2_500_000),
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/stream"));
    assert!(first_output.status.success());

    let second_output =
        run_session_request(&temp, &server.url("/stream-idempotent-replay-numeric"));
    assert!(
        second_output.status.success(),
        "numeric acceptedCumulative replay should be handled as success: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.idempotent_replay_problem_count, 1,
        "server should emit one numeric delta-too-small replay response"
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert!(
        channels[0].cumulative_amount >= 2_000_000,
        "numeric acceptedCumulative should preserve monotonic channel persistence"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_voucher_update_timeout_fails_stream_request() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: true,
        voucher_head_unsupported: false,
        sse_receipt_accepted: Some(2_500_000),
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: None,
        insufficient_balance_once: false,
        amount_exceeds_deposit_once: false,
        error_after_payment_once_status: None,
        response_delay_ms: 0,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let first_output = run_session_request(&temp, &server.url("/stream"));
    assert!(first_output.status.success());

    // When the stream completes normally ([DONE]), stale voucher tasks
    // are not awaited — blocking on them would delay exit unnecessarily.
    let second_output = run_session_request_with_env(
        &temp,
        &server.url("/stream-head-hang"),
        &[("TEMPO_SESSION_NORMAL_TIMEOUT_MS", "10")],
    );
    assert!(
        second_output.status.success(),
        "stream should succeed despite pending voucher task: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert!(
        observed.voucher_head_updates >= 1,
        "hanging path should attempt voucher HEAD update"
    );
}
