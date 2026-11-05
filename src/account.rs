//! Account management (port of `pymt5._account`): account info, passwords, demo/real
//! account opening, OTP, notifications, corporate links.

use tracing::debug;

use crate::client::{observed_build_release_date, trunc, Mt5Client, MT5_TERMINAL_VERSION};
use crate::constants::*;
use crate::error::Result;
use crate::parsers::{parse_account_response, parse_counted_records, parse_open_account_result, parse_verification_status};
use crate::protocol::{PropType, Record, RecordList, SerField, SeriesCodec};
use crate::schemas::*;
use crate::transport::CommandResult;
use crate::types::*;

impl Mt5Client {
    /// Full account info (`cmd=3`): balance, equity, margin, leverage, trade settings, ...
    pub async fn get_account(&self) -> Result<Record> {
        let r = self.transport().send_command(CMD_GET_ACCOUNT, &[]).await?;
        Ok(parse_account_response(&r.body))
    }

    /// Official-style alias for [`Mt5Client::get_account`].
    pub async fn account_info(&self) -> Result<Record> { self.get_account().await }

    /// Best-effort terminal/server info derived from the account config.
    pub async fn terminal_info(&self) -> Result<Record> {
        let a = self.get_account().await?;
        let trade_allowed = a.get_bool("trade_allowed");
        Ok(Record::new()
            .with("build", a.get_i64("server_build"))
            .with("company", a.get_str("company"))
            .with("name", a.get_str("server_name"))
            .with("server", a.get_str("server_name"))
            .with("connected", self.transport().is_ready())
            .with("trade_allowed", trade_allowed)
            .with("tradeapi_disabled", a.get_bool("is_read_only") || !trade_allowed)
            .with("timezone_shift", a.get_i64("timezone_shift"))
            .with("server_offset_time", a.get_i64("server_offset_time"))
            .with("path", "")
            .with("data_path", "")
            .with("commondata_path", ""))
    }

    /// Best-effort `(terminal_version, build, release_date)`; `None` (and `last_error`) if unknown.
    pub async fn version(&self) -> Result<Option<(i64, i64, String)>> {
        let account = match self.get_account().await {
            Ok(a) => a,
            Err(e @ (crate::Error::Protocol(_) | crate::Error::Validation(_))) => {
                self.fail_last_error(-99, format!("version() failed: {e}"));
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let build = account.get_i64("server_build");
        if build <= 0 {
            self.fail_last_error(-7, "terminal build unavailable for version()");
            return Ok(None);
        }
        self.clear_last_error();
        Ok(Some((MT5_TERMINAL_VERSION, build, observed_build_release_date(build).to_string())))
    }

    /// Account summary from cmd=3 plus position/order counts (falls back to position sums).
    pub async fn get_account_summary(&self) -> Result<AccountInfo> {
        let data = self.get_positions_and_orders().await?;
        match self.get_account().await {
            Ok(a) if !a.is_empty() => {
                return Ok(AccountInfo {
                    balance: a.get_f64("balance"),
                    equity: a.get_f64("equity"),
                    margin: a.get_f64("margin"),
                    margin_free: a.get_f64("margin_free"),
                    margin_level: a.get_f64("margin_level"),
                    profit: a.get_f64("profit"),
                    credit: a.get_f64("credit"),
                    leverage: a.get_i64("leverage"),
                    currency: a.get_str("currency").to_string(),
                    server: a.get_str("server").to_string(),
                    positions_count: data.positions.len(),
                    orders_count: data.orders.len(),
                })
            }
            Ok(_) => {}
            Err(e) => debug!("get_account failed, falling back to positions: {e}"),
        }
        let total: f64 = data.positions.iter().map(|p| p.get_f64("profit") + p.get_f64("commission") + p.get_f64("storage")).sum();
        Ok(AccountInfo { profit: total, positions_count: data.positions.len(), orders_count: data.orders.len(), ..Default::default() })
    }

    /// Change the (investor) password; returns the server's status code.
    pub async fn change_password(&self, new_password: &str, old_password: &str, is_investor: bool) -> Result<i32> {
        let payload = SeriesCodec::serialize(&[
            SerField::u8(is_investor as u8),
            SerField::fixed_string(&trunc(new_password, 32), 64),
            SerField::fixed_string(&trunc(old_password, 32), 64),
        ])?;
        let r = self.transport().send_command(CMD_CHANGE_PASSWORD, &payload).await?;
        let mut b = [0u8; 4];
        for (i, v) in r.body.iter().take(4).enumerate() { b[i] = *v; }
        Ok(i32::from_le_bytes(b))
    }

    pub async fn trader_params(&self) -> Result<(String, String)> {
        let r = self.transport().send_command(CMD_TRADER_PARAMS, &[]).await?;
        let v = SeriesCodec::parse(&r.body, TRADER_PARAMS_SCHEMA, 0)?;
        Ok((v[0].as_str().unwrap_or("").to_string(), v[1].as_str().unwrap_or("").to_string()))
    }

    /// Legacy init-style demo account request (cmd=30).
    pub async fn open_demo(&self, password: &str, otp: &str, cid: Option<&[u8]>, version: u32) -> Result<CommandResult> {
        let payload = self.build_init_payload(version, password, otp, cid)?;
        self.transport().send_command(CMD_OPEN_DEMO, &payload).await
    }

    pub async fn open_demo_account(&self, request: &DemoAccountRequest, cid: Option<&[u8]>, initialize: bool) -> Result<OpenAccountResult> {
        let client_id = self.resolve_client_id(cid)?;
        if initialize { self.init_session(0, "", "", Some(&client_id)).await?; }
        let payload = Self::build_opening_base_payload(request)?;
        let r = self.transport().send_command(CMD_OPEN_DEMO, &payload).await?;
        Ok(parse_open_account_result(&r.body))
    }

    pub async fn open_real_account(&self, request: &RealAccountRequest, cid: Option<&[u8]>, initialize: bool) -> Result<OpenAccountResult> {
        let client_id = self.resolve_client_id(cid)?;
        if initialize { self.init_session(0, "", "", Some(&client_id)).await?; }
        let payload = Self::build_real_account_payload(request)?;
        let r = self.transport().send_command(CMD_OPEN_REAL, &payload).await?;
        Ok(parse_open_account_result(&r.body))
    }

    /// Send a verification code (cmd=27), e.g. two-factor authentication.
    pub async fn verify_code(&self, code: &str) -> Result<CommandResult> {
        let payload = SeriesCodec::serialize(&[SerField::fixed_string(&trunc(code, 32), 64)])?;
        self.transport().send_command(CMD_VERIFY_CODE, &payload).await
    }

    /// Request email/SMS verification requirements for account opening (cmd=27).
    pub async fn request_opening_verification(
        &self,
        request: &AccountOpeningRequest,
        build: i16,
        cid: Option<&[u8]>,
        initialize: bool,
    ) -> Result<VerificationStatus> {
        let client_id = self.resolve_client_id(cid)?;
        if initialize { self.init_session(0, "", "", Some(&client_id)).await?; }
        let base = Self::build_opening_base_payload(request)?;
        let payload = SeriesCodec::serialize(&[SerField::i16(build), SerField::bytes(&client_id, 16), SerField::raw(&base)])?;
        let r = self.transport().send_command(CMD_VERIFY_CODE, &payload).await?;
        Ok(parse_verification_status(&r.body))
    }

    /// Submit email/SMS verification codes for account opening (cmd=40).
    pub async fn submit_opening_verification(&self, request: &AccountOpeningRequest, cid: Option<&[u8]>, initialize: bool) -> Result<VerificationStatus> {
        let client_id = self.resolve_client_id(cid)?;
        if initialize { self.init_session(0, "", "", Some(&client_id)).await?; }
        let payload = Self::build_opening_base_payload(request)?;
        let r = self.transport().send_command(CMD_SEND_VERIFY_CODES, &payload).await?;
        Ok(parse_verification_status(&r.body))
    }

    /// Enable/configure TOTP (cmd=43).
    pub async fn enable_otp(&self, login: u64, password: &str, otp_secret: &str, otp_secret_check: &str, cid: Option<&[u8]>) -> Result<CommandResult> {
        let payload = self.build_otp_setup_payload(login, password, "", otp_secret, otp_secret_check, cid)?;
        self.transport().send_command(CMD_OTP_SETUP, &payload).await
    }

    /// Disable TOTP (cmd=43). Any non-error response counts as success.
    pub async fn disable_otp(&self, login: u64, password: &str, otp: &str, cid: Option<&[u8]>) -> Result<bool> {
        let payload = self.build_otp_setup_payload(login, password, otp, "", "", cid)?;
        self.transport().send_command(CMD_OTP_SETUP, &payload).await?;
        Ok(true)
    }

    /// Send a notification message to the server (cmd=42).
    pub async fn send_notification(&self, message: &str) -> Result<CommandResult> {
        let payload = SeriesCodec::serialize(&[SerField::fixed_string(&trunc(message, 128), 256)])?;
        self.transport().send_command(CMD_NOTIFY, &payload).await
    }

    /// Broker corporate links (cmd=44): `link_type, url, label, flags, icon_data`.
    pub async fn get_corporate_links(&self) -> Result<RecordList> {
        let r = self.transport().send_command(CMD_GET_CORPORATE_LINKS, &[]).await?;
        Ok(parse_counted_records(&r.body, CORPORATE_LINK_SCHEMA, CORPORATE_LINK_FIELD_NAMES))
    }

    // ---- payload builders ----

    pub(crate) fn build_opening_base_payload(r: &AccountOpeningRequest) -> Result<Vec<u8>> {
        let full_name = [r.first_name.as_str(), r.second_name.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
        SeriesCodec::serialize(&[
            SerField::fixed_string(&trunc(&full_name, 128), 256),
            SerField::fixed_string(&trunc(&r.group, 64), 128),
            SerField::fixed_string(&trunc(&r.phone_password, 32), 64),
            SerField::fixed_string(&trunc(&r.country, 32), 64),
            SerField::fixed_string(&trunc(&r.city, 32), 64),
            SerField::fixed_string(&trunc(&r.state, 32), 64),
            SerField::fixed_string(&trunc(&r.zipcode, 16), 32),
            SerField::fixed_string(&trunc(&r.address, 128), 256),
            SerField::fixed_string(&trunc(&r.phone, 32), 64),
            SerField::fixed_string(&trunc(&r.email, 64), 128),
            SerField::f64(r.deposit),
            SerField::u32(r.leverage),
            SerField::u32(0),
            SerField::u32(1),
            SerField::fixed_string(&trunc(&r.domain, 64), 128),
            SerField::fixed_string(&trunc(&r.utm_campaign, 32), 64),
            SerField::fixed_string(&trunc(&r.utm_source, 32), 64),
            SerField::u32(r.email_confirm_code),
            SerField::u32(r.phone_confirm_code),
            SerField::fixed_string(&trunc(&r.first_name, 64), 128),
            SerField::fixed_string(&trunc(&r.second_name, 64), 128),
            SerField::u32(r.agreements),
        ])
    }

    fn build_document_payload(d: &AccountDocument) -> Result<Vec<u8>> {
        SeriesCodec::serialize(&[
            SerField::u32(d.data_type),
            SerField::u32(d.document_type),
            SerField::fixed_string(&trunc(&d.front_name, 260), 520),
            SerField::u32(d.front_buffer.len() as u32),
            SerField::fixed_string(&trunc(&d.back_name, 260), 520),
            SerField::u32(d.back_buffer.len() as u32),
            SerField::raw(&d.front_buffer),
            SerField::raw(&d.back_buffer),
        ])
    }

    pub(crate) fn build_real_account_payload(r: &RealAccountRequest) -> Result<Vec<u8>> {
        let mut out = Self::build_opening_base_payload(&r.base)?;
        let b = &r.base;
        out.extend(SeriesCodec::serialize(&[
            SerField::fixed_string(&trunc(&b.first_name, 64), 128),
            SerField::fixed_string(&trunc(&b.second_name, 64), 128),
            SerField::fixed_string(&trunc(&r.middle_name, 64), 128),
            SerField::new(PropType::Time, r.birth_date_ms, None),
            SerField::u32(r.gender),
            SerField::fixed_string(&trunc(&b.language, 64), 128),
            SerField::fixed_string(&trunc(&r.citizenship, 32), 64),
            SerField::fixed_string(&trunc(&r.tax_id, 64), 128),
            SerField::u32(r.employment),
            SerField::u32(r.industry),
            SerField::u32(r.education),
            SerField::u32(r.wealth),
            SerField::u64(r.annual_income),
            SerField::u64(r.net_worth),
            SerField::u64(r.annual_deposit),
            SerField::u32(r.experience_fx),
            SerField::u32(r.experience_cfd),
            SerField::u32(r.experience_futures),
            SerField::u32(r.experience_stocks),
            SerField::bytes(&REAL_ACCOUNT_RESERVED_PAYLOAD, 512),
        ])?);
        for d in &r.documents { out.extend(Self::build_document_payload(d)?); }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_payload_sizes() {
        let req = AccountOpeningRequest { first_name: "A".into(), second_name: "B".into(), ..Default::default() };
        let base = Mt5Client::build_opening_base_payload(&req).unwrap();
        let expect = 256 + 128 + 64 * 4 + 32 + 256 + 64 + 128 + 8 + 4 * 3 + 128 + 64 * 2 + 4 * 2 + 128 * 2 + 4;
        assert_eq!(base.len(), expect);
        let real = RealAccountRequest { base: req, documents: vec![AccountDocument { front_buffer: vec![1, 2, 3], ..Default::default() }], ..Default::default() };
        let p = Mt5Client::build_real_account_payload(&real).unwrap();
        assert!(p.len() > base.len() + 512);
    }
}
