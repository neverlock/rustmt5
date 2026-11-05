//! Data types (port of `pymt5.types` and `pymt5.events`).

use std::fmt;

use crate::protocol::Record;

/// Result of a trade request.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TradeResult {
    pub retcode: i64,
    pub description: String,
    pub success: bool,
    pub deal: u64,
    pub order: u64,
    pub volume: u64,
    pub price: f64,
    pub bid: f64,
    pub ask: f64,
    pub comment: String,
    pub request_id: u64,
}

impl TradeResult {
    /// Turn an unsuccessful result into [`crate::Error::Trade`] (`?`-friendly):
    /// `client.buy_market(..).await?.into_result("EURUSD", 1)?`. `action` is the `trade_action` sent.
    pub fn into_result(self, symbol: &str, action: u32) -> Result<TradeResult, crate::Error> {
        if self.success { return Ok(self); }
        Err(crate::Error::Trade { message: self.description, retcode: self.retcode.max(0) as u32, symbol: symbol.to_string(), action })
    }
}

impl fmt::Display for TradeResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = vec![format!("retcode={}", self.retcode), format!("success={}", self.success), format!("desc='{}'", self.description)];
        if self.deal != 0 { parts.push(format!("deal={}", self.deal)); }
        if self.order != 0 { parts.push(format!("order={}", self.order)); }
        if self.price != 0.0 { parts.push(format!("price={}", self.price)); }
        write!(f, "TradeResult({})", parts.join(", "))
    }
}

/// Account summary.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AccountInfo {
    pub balance: f64,
    pub equity: f64,
    pub margin: f64,
    pub margin_free: f64,
    pub margin_level: f64,
    pub profit: f64,
    pub credit: f64,
    pub leverage: i64,
    pub currency: String,
    pub server: String,
    pub positions_count: usize,
    pub orders_count: usize,
}

/// Cached basic symbol info.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SymbolInfo {
    pub name: String,
    pub symbol_id: u32,
    pub digits: u32,
    pub description: String,
    pub path: String,
    pub trade_calc_mode: i64,
    pub basis: String,
    pub sector: i64,
}

/// Email/phone verification state for account opening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VerificationStatus {
    pub email: bool,
    pub phone: bool,
}
impl VerificationStatus {
    /// Python `bool(status)`: true when either channel is set.
    pub fn any(&self) -> bool { self.email || self.phone }
}

/// Result of a demo/real account opening command.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpenAccountResult {
    pub code: i64,
    pub login: i64,
    pub password: String,
    pub investor_password: String,
}
impl OpenAccountResult {
    pub fn success(&self) -> bool { self.code == 0 }
}

/// Shared fields of the Web Terminal account opening payloads.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountOpeningRequest {
    pub first_name: String,
    pub second_name: String,
    pub email: String,
    pub phone: String,
    pub group: String,
    pub deposit: f64,
    pub leverage: u32,
    pub agreements: u32,
    pub country: String,
    pub city: String,
    pub state: String,
    pub zipcode: String,
    pub address: String,
    pub domain: String,
    pub phone_password: String,
    pub email_confirm_code: u32,
    pub phone_confirm_code: u32,
    pub language: String,
    pub utm_campaign: String,
    pub utm_source: String,
}

impl Default for AccountOpeningRequest {
    fn default() -> Self {
        Self {
            first_name: String::new(), second_name: String::new(), email: String::new(), phone: String::new(),
            group: String::new(), deposit: 100000.0, leverage: 100, agreements: 0, country: String::new(),
            city: String::new(), state: String::new(), zipcode: String::new(), address: String::new(),
            domain: String::new(), phone_password: String::new(), email_confirm_code: 0, phone_confirm_code: 0,
            language: String::new(), utm_campaign: String::new(), utm_source: String::new(),
        }
    }
}

/// Demo account request (cmd=30): identical to the shared base.
pub type DemoAccountRequest = AccountOpeningRequest;

/// Document upload payload for real account opening.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AccountDocument {
    pub data_type: u32,
    pub document_type: u32,
    pub front_name: String,
    pub front_buffer: Vec<u8>,
    pub back_name: String,
    pub back_buffer: Vec<u8>,
}

/// Real account request (cmd=39). `birth_date_ms` is Unix milliseconds.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RealAccountRequest {
    pub base: AccountOpeningRequest,
    pub middle_name: String,
    pub birth_date_ms: i64,
    pub gender: u32,
    pub citizenship: String,
    pub tax_id: String,
    pub employment: u32,
    pub industry: u32,
    pub education: u32,
    pub wealth: u32,
    pub annual_income: u64,
    pub net_worth: u64,
    pub annual_deposit: u64,
    pub experience_fx: u32,
    pub experience_cfd: u32,
    pub experience_futures: u32,
    pub experience_stocks: u32,
    pub documents: Vec<AccountDocument>,
}

/// 128 zeroed i32 values (512 bytes) reserved block of the real account payload.
pub const REAL_ACCOUNT_RESERVED_PAYLOAD: [u8; 512] = [0u8; 512];

// ---- Typed push events ----

#[derive(Debug, Clone, PartialEq)]
pub struct TickEvent {
    pub symbol_id: u32,
    pub symbol: String,
    pub bid: f64,
    pub ask: f64,
    pub last: f64,
    pub volume: f64,
    pub timestamp: f64,
    pub raw: Record,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BookEvent {
    pub symbol_id: u32,
    pub symbol: String,
    pub entries: Vec<Record>,
    pub raw: Record,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TradeResultEvent {
    pub retcode: i64,
    pub order: u64,
    pub deal: u64,
    pub volume: f64,
    pub price: f64,
    pub comment: String,
    pub raw: Record,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountEvent {
    pub balance: f64,
    pub equity: f64,
    pub margin: f64,
    pub margin_free: f64,
    pub raw: Record,
}

/// Snapshot of connection health metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthStatus {
    pub state: crate::transport::TransportState,
    pub ping_latency_ms: Option<f64>,
    pub last_message_age_secs: Option<f64>,
    pub uptime_seconds: f64,
    pub reconnect_count: u32,
}

#[cfg(test)]
mod trade_result_tests {
    use super::*;
    #[test]
    fn into_result_maps_failures() {
        let ok = TradeResult { retcode: 10009, success: true, ..Default::default() };
        assert!(ok.into_result("EURUSD", 1).is_ok());
        let bad = TradeResult { retcode: 10015, description: "Invalid price".into(), ..Default::default() };
        assert!(matches!(bad.into_result("EURUSD", 5), Err(crate::Error::Trade { retcode: 10015, .. })));
    }
}
