//! Boundary guarantees for behavior delegated to the `mpp` crate.
//!
//! These tests lock protocol-critical assumptions at our integration boundary so
//! upgrades to `mpp` cannot silently change client conformance behavior.

use alloy::{
    primitives::{Address, Signature, B256},
    signers::local::PrivateKeySigner,
};
use mpp::{
    protocol::methods::tempo::{session::SessionCredentialPayload, sign_voucher},
    Base64UrlJson, PaymentChallenge, PaymentCredential,
};
use serde_json::json;

fn verify_voucher_with_compact_signature_support(
    escrow_contract: Address,
    chain_id: u64,
    channel_id: B256,
    cumulative_amount: u128,
    signature_bytes: &[u8],
    expected_signer: Address,
) -> bool {
    let normalized_signature;
    let signature_for_mpp = if signature_bytes.len() == 64 {
        normalized_signature = Signature::from_erc2098(signature_bytes).as_bytes().to_vec();
        normalized_signature.as_slice()
    } else {
        signature_bytes
    };

    mpp::protocol::methods::tempo::voucher::verify_voucher(
        escrow_contract,
        chain_id,
        channel_id,
        cumulative_amount,
        signature_for_mpp,
        expected_signer,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voucher_signature_is_bound_to_eip712_domain_inputs() {
    let signer = PrivateKeySigner::random();
    let channel_id = B256::repeat_byte(0x11);
    let cumulative_amount = 42u128;
    let escrow_contract: Address = "0x5555555555555555555555555555555555555555"
        .parse()
        .unwrap();
    let chain_id = 42_431u64;

    let signature = sign_voucher(
        &signer,
        channel_id,
        cumulative_amount,
        escrow_contract,
        chain_id,
    )
    .await
    .expect("voucher signing should succeed");

    assert!(mpp::protocol::methods::tempo::voucher::verify_voucher(
        escrow_contract,
        chain_id,
        channel_id,
        cumulative_amount,
        &signature,
        signer.address(),
    ));

    let wrong_chain_id = chain_id + 1;
    assert!(
        !mpp::protocol::methods::tempo::voucher::verify_voucher(
            escrow_contract,
            wrong_chain_id,
            channel_id,
            cumulative_amount,
            &signature,
            signer.address(),
        ),
        "changing chain_id must invalidate voucher signature"
    );

    let wrong_escrow: Address = "0x4444444444444444444444444444444444444444"
        .parse()
        .unwrap();
    assert!(
        !mpp::protocol::methods::tempo::voucher::verify_voucher(
            wrong_escrow,
            chain_id,
            channel_id,
            cumulative_amount,
            &signature,
            signer.address(),
        ),
        "changing verifying_contract must invalidate voucher signature"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voucher_signature_accepts_65_byte_and_compact_erc2098_with_local_normalization() {
    let signer = PrivateKeySigner::random();
    let channel_id = B256::repeat_byte(0x22);
    let cumulative_amount = 7u128;
    let escrow_contract: Address = "0x7777777777777777777777777777777777777777"
        .parse()
        .unwrap();
    let chain_id = 42_431u64;

    let signature = sign_voucher(
        &signer,
        channel_id,
        cumulative_amount,
        escrow_contract,
        chain_id,
    )
    .await
    .expect("voucher signing should succeed");

    assert_eq!(
        signature.len(),
        65,
        "mpp signer should emit 65-byte signatures"
    );
    assert!(mpp::protocol::methods::tempo::voucher::verify_voucher(
        escrow_contract,
        chain_id,
        channel_id,
        cumulative_amount,
        &signature,
        signer.address(),
    ));

    let parsed = Signature::try_from(signature.as_ref()).expect("signature should parse");
    let compact_erc2098 = parsed.as_erc2098();

    assert!(
        !mpp::protocol::methods::tempo::voucher::verify_voucher(
            escrow_contract,
            chain_id,
            channel_id,
            cumulative_amount,
            &compact_erc2098,
            signer.address(),
        ),
        "mpp currently verifies canonical 65-byte signatures directly"
    );

    assert!(verify_voucher_with_compact_signature_support(
        escrow_contract,
        chain_id,
        channel_id,
        cumulative_amount,
        &compact_erc2098,
        signer.address(),
    ));
}

#[test]
fn mpp_parsing_tolerates_unknown_fields_for_session_boundary_types() {
    let session_request_wire = json!({
        "amount": "1000",
        "currency": "0x20c0000000000000000000000000000000000000",
        "recipient": "0x1111111111111111111111111111111111111111",
        "suggestedDeposit": "5000",
        "methodDetails": {
            "escrowContract": "0x2222222222222222222222222222222222222222",
            "chainId": 42431,
            "serverHint": "preserve-for-server"
        },
        "futureField": "ignored-by-client"
    });
    let request = Base64UrlJson::from_value(&session_request_wire).expect("request should encode");
    let challenge = PaymentChallenge::new("test-id", "test-realm", "tempo", "session", request);
    let challenge_header = mpp::format_www_authenticate(&challenge).expect("header should format");
    let parsed_challenge =
        mpp::parse_www_authenticate(&challenge_header).expect("challenge should parse");
    let parsed_request: mpp::SessionRequest = parsed_challenge
        .request
        .decode()
        .expect("request should decode");

    assert_eq!(parsed_request.amount, "1000");
    assert_eq!(
        parsed_request
            .method_details
            .as_ref()
            .expect("method details should be present")["serverHint"],
        "preserve-for-server"
    );

    let payload = json!({
        "action": "voucher",
        "channelId": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "cumulativeAmount": "123",
        "signature": "0xdeadbeef",
        "extraCredentialField": "ignored-by-parser"
    });
    let credential = PaymentCredential::with_source(
        challenge.to_echo(),
        "did:pkh:eip155:42431:0x1111111111111111111111111111111111111111",
        payload,
    );
    let authorization = mpp::format_authorization(&credential).expect("auth should format");
    let parsed_credential = mpp::parse_authorization(&authorization).expect("auth should parse");
    let parsed_payload: SessionCredentialPayload = parsed_credential
        .payload_as()
        .expect("session payload should parse");
    assert!(matches!(
        parsed_payload,
        SessionCredentialPayload::Voucher {
            channel_id,
            cumulative_amount,
            signature,
            ..
        } if channel_id.starts_with("0x") && cumulative_amount == "123" && signature == "0xdeadbeef"
    ));

    let receipt_wire = r#"{"status":"success","method":"tempo","timestamp":"2026-03-15T00:00:00Z","reference":"0xabc","additional":"ignored"}"#;
    let receipt_header = mpp::base64url_encode(receipt_wire.as_bytes());
    let parsed_receipt = mpp::parse_receipt(&receipt_header).expect("receipt should parse");
    assert_eq!(parsed_receipt.reference, "0xabc");
    assert_eq!(parsed_receipt.method.as_str(), "tempo");
}
