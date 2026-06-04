//! Wallet-specific analytics events and payloads.

use serde::Serialize;
use tempo_common::analytics::Event;

pub(crate) const LOGIN_STARTED: Event = Event::new("login started");
pub(crate) const LOGIN_SUCCESS: Event = Event::new("login succeeded");
pub(crate) const LOGIN_FAILURE: Event = Event::new("login failed");
pub(crate) const LOGIN_TIMEOUT: Event = Event::new("login timed out");
pub(crate) const LOGOUT: Event = Event::new("user logged out");
pub(crate) const KEY_CREATED: Event = Event::new("key created");
pub(crate) const WHOAMI_VIEWED: Event = Event::new("whoami viewed");
pub(crate) const CALLBACK_WINDOW_OPENED: Event = Event::new("callback window opened");
pub(crate) const CALLBACK_RECEIVED: Event = Event::new("callback received");
pub(crate) const WALLET_CREATED: Event = Event::new("wallet created");
pub(crate) const WALLET_FUND_STARTED: Event = Event::new("wallet fund started");
pub(crate) const WALLET_FUND_SUCCESS: Event = Event::new("wallet fund succeeded");
pub(crate) const WALLET_FUND_FAILURE: Event = Event::new("wallet fund failed");

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LoginFailurePayload {
    pub(crate) error: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CallbackReceivedPayload {
    pub(crate) duration_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WalletCreatedPayload {
    pub(crate) wallet_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WalletFundPayload {
    pub(crate) network: String,
    pub(crate) method: String,
    pub(crate) target: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WalletFundFailurePayload {
    pub(crate) network: String,
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) error: String,
}
