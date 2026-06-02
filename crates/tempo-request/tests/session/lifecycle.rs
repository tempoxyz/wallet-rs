//! Channel lifecycle and reuse regression scenarios split from commands.rs.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_spend_below_challenge_amount_fails_before_open() {
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

    let output =
        run_session_request_with_args(&temp, &server.url("/resource"), &["--max-spend", "0.0005"]);
    assert!(
        !output.status.success(),
        "request should fail when max-spend is below required session amount: {}",
        get_combined_output(&output)
    );

    let combined = get_combined_output(&output);
    assert!(
        combined.contains("Payment max spend exceeded"),
        "error should explain max-spend breach: {combined}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.open_count, 0,
        "max-spend precheck should prevent opening a channel"
    );
    assert_eq!(observed.voucher_count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_spend_caps_reused_session_cumulative_spend() {
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

    let first_output =
        run_session_request_with_args(&temp, &server.url("/resource"), &["--max-spend", "1"]);
    assert!(
        first_output.status.success(),
        "first request should succeed within max-spend: {}",
        get_combined_output(&first_output)
    );

    let second_output =
        run_session_request_with_args(&temp, &server.url("/resource"), &["--max-spend", "1"]);
    assert!(
        !second_output.status.success(),
        "second request should fail when next cumulative exceeds max-spend: {}",
        get_combined_output(&second_output)
    );

    let combined = get_combined_output(&second_output);
    assert!(
        combined.contains("Payment max spend exceeded"),
        "error should explain cumulative max-spend breach: {combined}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.open_count, 1,
        "reused flow should not open a second channel"
    );
    assert_eq!(
        observed.voucher_count, 0,
        "max-spend check should fail before posting a voucher"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_spend_sets_open_deposit_budget() {
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

    let output =
        run_session_request_with_args(&temp, &server.url("/resource"), &["--max-spend", "2"]);
    assert!(
        output.status.success(),
        "request should succeed with explicit max-spend budget: {}",
        get_combined_output(&output)
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1, "exactly one channel should be persisted");
    assert_eq!(
        channels[0].deposit, 2_000_000,
        "open deposit should match max-spend budget"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opens_channel_persists_state_and_reuses_authorized_session() {
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
    assert!(
        first_output.status.success(),
        "first request should succeed: {}",
        get_combined_output(&first_output)
    );

    let after_first = server.snapshot();
    assert_eq!(
        after_first.open_count, 1,
        "first request should open a channel"
    );
    assert_eq!(
        after_first.voucher_count, 0,
        "first request should not reuse voucher"
    );
    assert_eq!(after_first.open_transactions.len(), 1);
    assert!(
        after_first.open_transactions[0].starts_with("0x"),
        "open credential should carry signed transaction bytes"
    );

    let channels_after_first = load_channels(&temp);
    assert_eq!(
        channels_after_first.len(),
        1,
        "exactly one channel should be persisted"
    );
    assert_eq!(channels_after_first[0].state, "active");
    assert_eq!(channels_after_first[0].cumulative_amount, SESSION_AMOUNT);

    let second_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        second_output.status.success(),
        "second request should succeed: {}",
        get_combined_output(&second_output)
    );

    let after_second = server.snapshot();
    assert_eq!(
        after_second.open_count, 1,
        "reuse should not trigger a second open tx"
    );
    assert_eq!(
        after_second.voucher_count, 1,
        "second request should use voucher replay"
    );
    assert_eq!(
        after_second.voucher_cumulative,
        vec![SESSION_AMOUNT * 2],
        "reused request should advance cumulative amount"
    );
    assert_eq!(
        rpc.snapshot().eth_call_count,
        0,
        "session voucher reuse should not validate the channel on-chain"
    );

    let channels_after_second = load_channels(&temp);
    assert_eq!(
        channels_after_second.len(),
        1,
        "reuse should keep one channel row"
    );
    assert_eq!(
        channels_after_second[0].cumulative_amount,
        SESSION_AMOUNT * 2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn varying_challenge_amount_reuses_same_channel_identity() {
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

    let first_output = run_session_request(&temp, &server.url("/amount-1x"));
    assert!(
        first_output.status.success(),
        "first request should succeed: {}",
        get_combined_output(&first_output)
    );

    let second_output = run_session_request(&temp, &server.url("/amount-2x"));
    assert!(
        second_output.status.success(),
        "second request should succeed with different challenge amount: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.open_count, 1,
        "changing challenge amount must not force a new channel open"
    );
    assert_eq!(
        observed.voucher_count, 1,
        "second request should reuse with voucher replay"
    );
    assert_eq!(
        observed.voucher_cumulative,
        vec![SESSION_AMOUNT * 3],
        "voucher cumulative should advance by the new request amount"
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1, "reuse should keep a single channel row");
    assert_eq!(
        channels[0].cumulative_amount,
        SESSION_AMOUNT * 3,
        "persisted cumulative should include both challenge amounts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payee_mismatch_forces_new_open() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::ByPath,
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

    let first_output = run_session_request(&temp, &server.url("/payee-a"));
    assert!(
        first_output.status.success(),
        "first request should succeed: {}",
        get_combined_output(&first_output)
    );

    let second_output = run_session_request(&temp, &server.url("/payee-b"));
    assert!(
        second_output.status.success(),
        "second request should succeed: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.open_count, 2,
        "payee mismatch must prevent channel reuse"
    );
    assert_eq!(
        observed.voucher_count, 0,
        "mismatched payee should not attempt voucher reuse"
    );

    let channels = load_channels(&temp);
    assert_eq!(
        channels.len(),
        2,
        "two separate channels should be persisted"
    );
    assert_ne!(
        channels[0].channel_id, channels[1].channel_id,
        "payee mismatch should create distinct channel ids"
    );
    let payees: Vec<String> = channels.into_iter().map(|channel| channel.payee).collect();
    assert!(
        payees.contains(&PAYEE_A.to_string()) && payees.contains(&PAYEE_B.to_string()),
        "persisted channels should retain distinct payees"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_active_state_forces_new_open() {
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
    assert!(
        first_output.status.success(),
        "first request should succeed: {}",
        get_combined_output(&first_output)
    );

    set_all_channel_state(&temp, "closing");

    let second_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        second_output.status.success(),
        "second request should succeed: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(observed.open_count, 2, "closing channel must not be reused");
    assert_eq!(
        observed.voucher_count, 0,
        "closing channel should skip voucher replay path"
    );

    let channels = load_channels(&temp);
    assert_eq!(
        channels.len(),
        2,
        "new open should create a second channel row"
    );
    assert_ne!(
        channels[0].channel_id, channels[1].channel_id,
        "non-active guardrail should create distinct channel ids"
    );
    let states: Vec<String> = channels.into_iter().map(|channel| channel.state).collect();
    assert!(
        states.contains(&"closing".to_string()) && states.contains(&"active".to_string()),
        "expected one preserved closing row and one newly active row"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_receipt_persists_and_sets_next_reuse_baseline() {
    let rpc = SessionRpcServer::start().await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: Some(5_000_000),
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
    assert!(
        first_output.status.success(),
        "first request should succeed: {}",
        get_combined_output(&first_output)
    );

    let channels_after_first = load_channels(&temp);
    assert_eq!(channels_after_first.len(), 1);
    assert_eq!(
        channels_after_first[0].cumulative_amount, 5_000_000,
        "open response receipt acceptedCumulative should be persisted"
    );

    let second_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        second_output.status.success(),
        "second request should succeed: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.open_count, 1,
        "second request should reuse existing channel"
    );
    assert_eq!(observed.voucher_count, 1);
    assert_eq!(
        observed.voucher_cumulative,
        vec![6_000_000],
        "next voucher baseline should use persisted accepted cumulative"
    );

    let channels_after_second = load_channels(&temp);
    assert_eq!(channels_after_second.len(), 1);
    assert_eq!(channels_after_second[0].cumulative_amount, 6_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_voucher_flow_falls_back_to_post_and_persists_receipt() {
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
        "first request should succeed: {}",
        get_combined_output(&first_output)
    );

    let second_output = run_session_request(&temp, &server.url("/stream"));
    assert!(
        second_output.status.success(),
        "SSE request should succeed: {}",
        get_combined_output(&second_output)
    );
    let second_stdout = String::from_utf8_lossy(&second_output.stdout);
    assert!(
        second_stdout.contains("stream"),
        "SSE message payload should be emitted to stdout: {second_stdout}"
    );

    let observed = server.snapshot();
    assert!(
        observed.voucher_head_updates >= 1,
        "SSE voucher flow should attempt HEAD transport first"
    );
    assert!(
        observed.voucher_post_updates >= 1,
        "SSE voucher flow should fall back to POST when HEAD is unsupported"
    );

    let channels = load_channels(&temp);
    assert_eq!(
        channels.len(),
        1,
        "SSE reuse should continue on the same channel"
    );
    assert_eq!(
        channels[0].cumulative_amount, 2_500_000,
        "payment-receipt SSE event acceptedCumulative should persist to channels.db"
    );
}

async fn run_invalidating_problem_case(
    problem_type: &'static str,
    rpc_config: SessionRpcConfig,
) -> (
    tempfile::TempDir,
    SessionObservations,
    std::process::Output,
    String,
) {
    let rpc = SessionRpcServer::start_with_config(rpc_config).await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: false,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: Some(problem_type),
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
        "first request should succeed: {}",
        get_combined_output(&first_output)
    );
    let channels_after_first = load_channels(&temp);
    assert_eq!(channels_after_first.len(), 1);
    let first_channel_id = channels_after_first[0].channel_id.clone();

    let second_output = run_session_request(&temp, &server.url("/resource"));
    let observed = server.snapshot();
    assert_eq!(
        observed.invalidating_problem_count, 1,
        "problem+json 410 should be emitted exactly once"
    );
    assert_eq!(
        observed.voucher_count, 1,
        "reuse path should attempt a single voucher on the invalidated channel"
    );

    (temp, observed, second_output, first_channel_id)
}

async fn run_invalidating_problem_reopen_case(problem_type: &'static str) {
    let (temp, observed, second_output, first_channel_id) = run_invalidating_problem_case(
        problem_type,
        SessionRpcConfig {
            channel_mode: SessionRpcChannelMode::ActiveThenMissingAfterEthCalls { threshold: 1 },
        },
    )
    .await;

    assert!(
        second_output.status.success(),
        "second request should reopen cleanly once on-chain invalidation is confirmed: {}",
        get_combined_output(&second_output)
    );

    assert_eq!(
        observed.open_count, 2,
        "invalidated channel should trigger opening a replacement channel"
    );

    let channels_after_second = load_channels(&temp);
    assert_eq!(
        channels_after_second.len(),
        1,
        "invalidated local channel should be replaced, not duplicated"
    );
    assert_ne!(
        channels_after_second[0].channel_id, first_channel_id,
        "replacement session should persist a new channel id"
    );
}

async fn run_invalidating_problem_unconfirmed_case(problem_type: &'static str) {
    let (temp, observed, second_output, first_channel_id) = run_invalidating_problem_case(
        problem_type,
        SessionRpcConfig {
            channel_mode: SessionRpcChannelMode::Active,
        },
    )
    .await;

    assert!(
        !second_output.status.success(),
        "second request should fail closed when invalidation is not confirmed on-chain: {}",
        get_combined_output(&second_output)
    );

    let stderr = String::from_utf8_lossy(&second_output.stderr);
    assert!(
        stderr.contains("not confirmed on-chain"),
        "error should explain fail-closed invalidation behavior: {stderr}"
    );

    assert_eq!(
        observed.open_count, 1,
        "unconfirmed invalidation must not trigger channel reopen"
    );

    let channels_after_second = load_channels(&temp);
    assert_eq!(
        channels_after_second.len(),
        1,
        "unconfirmed invalidation should preserve local channel record"
    );
    assert_eq!(
        channels_after_second[0].channel_id, first_channel_id,
        "unconfirmed invalidation must not replace the persisted channel"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_not_found_problem_with_delete_failure_fails_closed_without_reopen() {
    let rpc = SessionRpcServer::start_with_config(SessionRpcConfig {
        channel_mode: SessionRpcChannelMode::ActiveThenMissingAfterEthCalls { threshold: 1 },
    })
    .await;
    let server = SessionServer::start(SessionServerConfig {
        payee_mode: PayeeMode::Fixed,
        open_receipt_accepted: None,
        sse_voucher_flow: false,
        voucher_head_unsupported: false,
        sse_receipt_accepted: None,
        sse_required_cumulative: None,
        sse_reported_deposit: None,
        invalidating_problem_type_once: Some(
            "https://paymentauth.org/problems/session/channel-not-found",
        ),
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
        "first request should succeed: {}",
        get_combined_output(&first_output)
    );
    let channels_after_first = load_channels(&temp);
    assert_eq!(channels_after_first.len(), 1);
    let first_channel_id = channels_after_first[0].channel_id.clone();

    let db_path = temp.path().join(".tempo/wallet/channels.db");
    let db_lock = rusqlite::Connection::open(&db_path).unwrap();
    db_lock
        .busy_timeout(std::time::Duration::from_millis(0))
        .unwrap();
    db_lock.execute_batch("BEGIN IMMEDIATE;").unwrap();

    let second_output = std::thread::scope(|scope| {
        let handle = scope.spawn(|| run_session_request(&temp, &server.url("/resource")));

        // Hold the write lock past the store busy_timeout so delete_channel fails.
        std::thread::sleep(std::time::Duration::from_millis(6_200));
        db_lock.execute_batch("ROLLBACK;").unwrap();

        handle.join().unwrap()
    });

    assert!(
        !second_output.status.success(),
        "delete failure on confirmed invalidation should fail closed: {}",
        get_combined_output(&second_output)
    );
    let stderr = String::from_utf8_lossy(&second_output.stderr);
    assert!(
        stderr.contains("Failed to remove invalidated channel before reopening")
            || stderr.contains("delete channel"),
        "error should explain invalidated-channel cleanup failure: {stderr}"
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.open_count, 1,
        "cleanup failure must not reopen or replace the channel"
    );
    assert_eq!(
        observed.voucher_count, 1,
        "reuse path should still attempt a single voucher before failing"
    );

    let channels_after_second = load_channels(&temp);
    assert_eq!(
        channels_after_second.len(),
        1,
        "cleanup failure should preserve exactly one local channel row"
    );
    assert_eq!(
        channels_after_second[0].channel_id, first_channel_id,
        "cleanup failure must keep the original channel persisted for audit/dispute"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_not_found_problem_triggers_reopen() {
    run_invalidating_problem_reopen_case(
        "https://paymentauth.org/problems/session/channel-not-found",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_not_found_problem_without_on_chain_confirmation_fails_closed() {
    run_invalidating_problem_unconfirmed_case(
        "https://paymentauth.org/problems/session/channel-not-found",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_finalized_problem_triggers_reopen() {
    run_invalidating_problem_reopen_case(
        "https://paymentauth.org/problems/session/channel-finalized",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_finalized_problem_without_on_chain_confirmation_fails_closed() {
    run_invalidating_problem_unconfirmed_case(
        "https://paymentauth.org/problems/session/channel-finalized",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insufficient_balance_problem_runs_structured_top_up_recovery() {
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
        "first request should succeed: {}",
        get_combined_output(&first_output)
    );

    let second_output = run_session_request(&temp, &server.url("/resource"));
    assert!(
        second_output.status.success(),
        "second request should recover via top-up: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.insufficient_balance_problem_count, 1,
        "insufficient-balance problem should be emitted exactly once"
    );
    assert_eq!(
        observed.top_up_count, 1,
        "client should submit one structured top-up credential"
    );
    assert_eq!(
        observed.voucher_count, 2,
        "client should retry voucher after successful top-up"
    );
    assert_eq!(
        observed.open_count, 1,
        "top-up recovery should stay on the same channel without reopening"
    );

    let channels = load_channels(&temp);
    assert_eq!(channels.len(), 1);
    assert_eq!(
        channels[0].cumulative_amount,
        SESSION_AMOUNT * 2,
        "successful post-top-up voucher should persist updated cumulative"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_origin_requests_do_not_double_open() {
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
        response_delay_ms: 250,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let (first_output, second_output) =
        run_two_concurrent_session_requests(&temp, &server.url("/resource"));
    assert!(
        first_output.status.success(),
        "first concurrent request should succeed: {}",
        get_combined_output(&first_output)
    );
    assert!(
        second_output.status.success(),
        "second concurrent request should succeed: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.open_count, 1,
        "concurrent requests on same origin should not double-open"
    );
    assert_eq!(
        observed.voucher_count, 1,
        "one concurrent request should reuse the channel via voucher"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_lock_file_does_not_block_reuse() {
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

    let request_url = server.url("/resource");
    let origin = url::Url::parse(&request_url)
        .unwrap()
        .origin()
        .ascii_serialization();
    let lock_path = temp
        .path()
        .join(".tempo/wallet")
        .join(format!("{}.lock", payment_origin_lock_key(&origin)));
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    std::fs::write(&lock_path, b"stale-lock-file").unwrap();

    let first_output = run_session_request(&temp, &request_url);
    assert!(
        first_output.status.success(),
        "first request should succeed with pre-existing lock file: {}",
        get_combined_output(&first_output)
    );
    let second_output = run_session_request(&temp, &request_url);
    assert!(
        second_output.status.success(),
        "second request should reuse despite stale lock file: {}",
        get_combined_output(&second_output)
    );

    let observed = server.snapshot();
    assert_eq!(
        observed.open_count, 1,
        "stale lock file should not force a second open"
    );
    assert_eq!(
        observed.voucher_count, 1,
        "channel should become reusable after stale lock file path"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_writers_preserve_single_row_and_progress_cumulative() {
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
        response_delay_ms: 250,
    })
    .await;

    let temp = tempfile::TempDir::new().unwrap();
    setup_config_only(&temp, &rpc.base_url);

    let (first_output, second_output) =
        run_two_concurrent_session_requests(&temp, &server.url("/resource"));
    assert!(first_output.status.success());
    assert!(second_output.status.success());

    let channels = load_channels(&temp);
    assert_eq!(
        channels.len(),
        1,
        "concurrent writers should preserve exactly one persisted channel"
    );
    assert_eq!(
        channels[0].cumulative_amount,
        SESSION_AMOUNT * 2,
        "concurrent write path should retain cumulative progression"
    );
}
