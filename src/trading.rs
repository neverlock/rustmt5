//! Trading: positions/orders, history, order calc/check/send (port of `pymt5._trading`).

use crate::currency::margin_rate_factor;
use tracing::{debug, info, warn};

use crate::client::{trunc, Mt5Client};
use crate::constants::*;
use crate::error::{Error, Result};
use crate::parsers::*;
use crate::protocol::{get_series_size, PropType, Record, RecordList, SerField, SeriesCodec};
use crate::push_handlers::split_positions_orders;
use crate::schemas::*;
use crate::types::TradeResult;

/// Open positions and pending orders (cmd=4).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PositionsAndOrders {
    pub positions: RecordList,
    pub orders: RecordList,
}

/// Closed deals and historical orders (cmd=5).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TradeHistory {
    pub deals: RecordList,
    pub orders: RecordList,
}

/// Optional filters for `history_*_get` (all `None` = no filter).
#[derive(Debug, Clone, Default)]
pub struct HistoryFilter {
    pub group: Option<String>,
    pub ticket: Option<i64>,
    pub position: Option<i64>,
}

/// Parameters of a raw trade request (cmd=12). `volume` is the integer lot encoding (lots * 1e8).
#[derive(Debug, Clone, Default)]
pub struct TradeRequest {
    pub action_id: u32,
    pub trade_action: i64,
    pub symbol: String,
    pub volume: u64,
    pub digits: u32,
    pub order: u64,
    pub trade_type: i64,
    pub type_filling: i64,
    pub type_time: i64,
    pub type_flags: i64,
    pub type_reason: i64,
    pub price_order: f64,
    pub price_trigger: f64,
    pub price_sl: f64,
    pub price_tp: f64,
    /// Serialised as u32 (the Web Terminal layout).
    pub deviation: u64,
    pub price_top: f64,
    pub price_bottom: f64,
    pub comment: String,
    pub position_id: u64,
    pub position_by: u64,
    pub time_expiration: i64,
}

fn by_symbol_group_ticket(items: RecordList, symbol: Option<&str>, group: Option<&str>, ticket: Option<i64>, ticket_key: &str) -> RecordList {
    if let Some(s) = symbol { return items.into_iter().filter(|i| i.get_str("trade_symbol") == s).collect(); }
    if let Some(g) = group { return items.into_iter().filter(|i| matches_group_mask(i.get_str("trade_symbol"), g)).collect(); }
    if let Some(t) = ticket { return items.into_iter().filter(|i| i.get_i64(ticket_key) == t).collect(); }
    items
}

fn apply_history_filter(items: RecordList, f: &HistoryFilter) -> RecordList {
    items
        .into_iter()
        .filter(|i| f.group.as_deref().is_none_or(|g| matches_group_mask(i.get_str("trade_symbol"), g)))
        .filter(|i| f.ticket.is_none_or(|t| i.get_i64("trade_order") == t))
        .filter(|i| f.position.is_none_or(|p| i.get_i64("position_id") == p))
        .collect()
}

impl Mt5Client {
    /// `lots * 10^precision` as the integer volume the Web Terminal expects (default precision 8).
    pub fn volume_to_lots(volume: f64, precision: i32) -> i64 { (volume * 10f64.powi(precision)).round_ties_even() as i64 }


    /// Open positions and pending orders.
    ///
    /// The first call after login seeds the live state from cmd 4; afterwards the state is kept current by
    /// cmd 10 pushes (some servers keep answering cmd 4 with the login-time snapshot, see docs/trading.md).
    /// Floating `profit`/`price_close` of positions are those of the last snapshot/push, not recomputed.
    pub async fn get_positions_and_orders(&self) -> Result<PositionsAndOrders> {
        let r = self.transport().send_command(CMD_GET_POSITIONS_ORDERS, &[]).await?;
        let (positions, orders) = split_positions_orders(&r.body);
        let mut st = self.st();
        if !st.trade_state_ready {
            st.live_positions = positions.iter().map(|p| (p.get_u64("position_id"), p.clone())).collect();
            st.live_orders = orders.iter().map(|o| (o.get_u64("trade_order"), o.clone())).collect();
            st.trade_state_ready = true;
        }
        let mut positions: RecordList = st.live_positions.values().cloned().collect();
        let orders: RecordList = st.live_orders.values().cloned().collect();
        drop(st);
        self.reprice_positions(&mut positions);
        Ok(PositionsAndOrders { positions, orders })
    }

    /// Refresh `price_close` and `profit` of position records from the cached ticks (buy closes at bid, sell at ask).
    /// Needs the full symbol spec (contract size etc.) in the cache and `rate_profit` on the record; otherwise the
    /// push values stay.
    fn reprice_positions(&self, positions: &mut RecordList) {
        for p in positions.iter_mut() {
            let symbol = p.get_str("trade_symbol").to_string();
            let rate = p.get_f64("rate_profit");
            let Some(tick) = self.symbol_info_tick(&symbol) else { continue };
            let Some(info) = self.st().full_symbols.get(&symbol).cloned() else { continue };
            let is_buy = p.get_i64("trade_action") == POSITION_TYPE_BUY;
            let now = if is_buy { tick.get_f64("bid") } else { tick.get_f64("ask") };
            if now <= 0.0 || rate <= 0.0 { continue; }
            let lots = p.get_f64("trade_volume") / 10f64.powi(self.volume_precision());
            if let Some(raw) = self.calc_profit_raw(&info, is_buy, lots, p.get_f64("price_open"), now) {
                p.set("price_close", now);
                p.set("profit", raw * rate);
            }
        }
    }

    /// Discard the live state and reload it from the server's cmd 4 answer. Only useful on servers that answer
    /// cmd 4 with current data; on build 6182 the answer is the (stale) login-time snapshot.
    pub async fn reload_positions_and_orders(&self) -> Result<PositionsAndOrders> {
        self.st().trade_state_ready = false;
        self.get_positions_and_orders().await
    }

    /// Closed deals and orders between two unix-second timestamps (`0` = unbounded).
    pub async fn get_trade_history(&self, from_ts: i32, to_ts: i32) -> Result<TradeHistory> {
        let mut payload = from_ts.to_le_bytes().to_vec();
        payload.extend(to_ts.to_le_bytes());
        let r = self.transport().send_command(CMD_GET_TRADE_HISTORY, &payload).await?;
        let body = &r.body;
        let deal_size = get_series_size(DEAL_SCHEMA)?;
        let count = body.get(0..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0) as usize;
        let mut deals = Vec::new();
        let mut offset = 4usize;
        for _ in 0..count {
            let mut d = SeriesCodec::parse_record(body, DEAL_SCHEMA, DEAL_FIELD_NAMES, offset)?;
            let tc = d.get_i64("time_create") * 1000 + d.get_i64("time_create_ms");
            let tu = d.get_i64("time_update") * 1000 + d.get_i64("time_update_ms");
            d.set("time_create", tc);
            d.set("time_update", tu);
            deals.push(d);
            offset += deal_size;
        }
        let orders = parse_counted_records(body.get(offset..).unwrap_or(&[]), ORDER_SCHEMA, ORDER_FIELD_NAMES);
        Ok(TradeHistory { deals, orders })
    }

    pub async fn get_positions(&self) -> Result<RecordList> { Ok(self.get_positions_and_orders().await?.positions) }
    pub async fn get_orders(&self) -> Result<RecordList> { Ok(self.get_positions_and_orders().await?.orders) }
    pub async fn get_deals(&self, from_ts: i32, to_ts: i32) -> Result<RecordList> { Ok(self.get_trade_history(from_ts, to_ts).await?.deals) }

    pub async fn positions_total(&self) -> Result<usize> { Ok(self.get_positions().await?.len()) }
    pub async fn orders_total(&self) -> Result<usize> { Ok(self.get_orders().await?.len()) }

    /// Filtered positions: by `symbol`, else `group` mask, else `ticket` (first given wins).
    pub async fn positions_get(&self, symbol: Option<&str>, group: Option<&str>, ticket: Option<i64>) -> Result<RecordList> {
        Ok(by_symbol_group_ticket(self.get_positions().await?, symbol, group, ticket, "position_id"))
    }

    pub async fn orders_get(&self, symbol: Option<&str>, group: Option<&str>, ticket: Option<i64>) -> Result<RecordList> {
        Ok(by_symbol_group_ticket(self.get_orders().await?, symbol, group, ticket, "trade_order"))
    }

    pub async fn history_orders_get(&self, date_from: Option<TimeArg>, date_to: Option<TimeArg>, filter: &HistoryFilter) -> Result<RecordList> {
        let h = self.get_trade_history(coerce_optional_timestamp(date_from) as i32, coerce_optional_timestamp(date_to) as i32).await?;
        Ok(apply_history_filter(h.orders, filter))
    }

    pub async fn history_orders_total(&self, date_from: Option<TimeArg>, date_to: Option<TimeArg>) -> Result<usize> {
        Ok(self.history_orders_get(date_from, date_to, &HistoryFilter::default()).await?.len())
    }

    pub async fn history_deals_get(&self, date_from: Option<TimeArg>, date_to: Option<TimeArg>, filter: &HistoryFilter) -> Result<RecordList> {
        let deals = self.get_deals(coerce_optional_timestamp(date_from) as i32, coerce_optional_timestamp(date_to) as i32).await?;
        Ok(apply_history_filter(deals, filter))
    }

    pub async fn history_deals_total(&self, date_from: Option<TimeArg>, date_to: Option<TimeArg>) -> Result<usize> {
        Ok(self.history_deals_get(date_from, date_to, &HistoryFilter::default()).await?.len())
    }

    /// Best-effort profit calculation from local symbol formulas. `None` + `last_error()` on failure.
    pub async fn order_calc_profit(&self, action: i64, symbol: &str, volume: f64, price_open: f64, price_close: f64) -> Result<Option<f64>> {
        match self.order_calc_profit_inner(action, symbol, volume, price_open, price_close).await {
            Err(e @ (Error::Protocol(_) | Error::Validation(_) | Error::SymbolNotFound(_))) => {
                self.fail_last_error(-99, format!("order_calc_profit() failed: {e}"));
                Ok(None)
            }
            other => other,
        }
    }

    async fn order_calc_profit_inner(&self, action: i64, symbol: &str, volume: f64, price_open: f64, price_close: f64) -> Result<Option<f64>> {
        let Some(is_buy) = order_side(action) else {
            self.fail_last_error(-1, format!("unsupported action for order_calc_profit(): {action}"));
            return Ok(None);
        };
        let Some(info) = self.symbol_info(symbol).await? else {
            self.fail_last_error(-2, format!("symbol not found: {symbol}"));
            return Ok(None);
        };
        let account = self.get_account().await?;
        let account_currency = account.get_str("currency").to_string();
        if account_currency.is_empty() { self.fail_last_error(-3, "account currency unavailable"); return Ok(None); }
        let Some(raw) = self.calc_profit_raw(&info, is_buy, volume, price_open, price_close) else { return Ok(None) };
        if raw == 0.0 { self.clear_last_error(); return Ok(Some(0.0)); }
        let profit_currency = info.get_str("currency_profit").to_string();
        if profit_currency.is_empty() { self.fail_last_error(-4, format!("symbol {symbol} is missing currency_profit")); return Ok(None); }
        if currencies_equal(&profit_currency, &account_currency) { self.clear_last_error(); return Ok(Some(raw)); }
        let current = { let s = info.get_str("trade_symbol"); if s.is_empty() { symbol } else { s }.to_string() };
        let Some((rate_buy, rate_sell)) = self.resolve_conversion_rates(&profit_currency, &account_currency, &current, price_close).await? else {
            self.fail_last_error(-5, format!("unable to convert profit currency {profit_currency} to {account_currency}"));
            return Ok(None);
        };
        let mode = info.get_i64("trade_calc_mode");
        let profit = if FOREX_CALC_MODES.contains(&mode) {
            raw * if is_buy { rate_buy } else { rate_sell }
        } else {
            raw * if raw > 0.0 { rate_sell } else { rate_buy }
        };
        self.clear_last_error();
        Ok(Some(profit))
    }

    /// Best-effort margin calculation from local symbol formulas. `None` + `last_error()` on failure.
    pub async fn order_calc_margin(&self, action: i64, symbol: &str, volume: f64, price: f64) -> Result<Option<f64>> {
        match self.order_calc_margin_inner(action, symbol, volume, price).await {
            Err(e @ (Error::Protocol(_) | Error::Validation(_) | Error::SymbolNotFound(_))) => {
                self.fail_last_error(-99, format!("order_calc_margin() failed: {e}"));
                Ok(None)
            }
            other => other,
        }
    }

    async fn order_calc_margin_inner(&self, action: i64, symbol: &str, volume: f64, price: f64) -> Result<Option<f64>> {
        let Some(is_buy) = order_side(action) else {
            self.fail_last_error(-1, format!("unsupported action for order_calc_margin(): {action}"));
            return Ok(None);
        };
        let Some(info) = self.symbol_info(symbol).await? else {
            self.fail_last_error(-2, format!("symbol not found: {symbol}"));
            return Ok(None);
        };
        let account = self.get_account().await?;
        let account_currency = account.get_str("currency").to_string();
        if account_currency.is_empty() { self.fail_last_error(-3, "account currency unavailable"); return Ok(None); }
        let leverage = account.get_i64("leverage");
        let Some(raw) = self.calc_margin_raw(&info, volume, price, leverage) else { return Ok(None) };
        let raw = raw * margin_rate_factor(&info, &account, volume, is_buy);
        if raw == 0.0 { self.clear_last_error(); return Ok(Some(0.0)); }
        let margin_currency = { let m = info.get_str("currency_margin"); if m.is_empty() { account_currency.clone() } else { m.to_string() } };
        if currencies_equal(&margin_currency, &account_currency) { self.clear_last_error(); return Ok(Some(raw)); }
        let current = { let s = info.get_str("trade_symbol"); if s.is_empty() { symbol } else { s }.to_string() };
        let Some((rate_buy, rate_sell)) = self.resolve_conversion_rates(&margin_currency, &account_currency, &current, price).await? else {
            self.fail_last_error(-6, format!("unable to convert margin currency {margin_currency} to {account_currency}"));
            return Ok(None);
        };
        self.clear_last_error();
        Ok(Some(raw * if is_buy { rate_buy } else { rate_sell }))
    }

    /// Send a raw trade request (cmd=12) after local sanity checks.
    pub async fn trade_request(&self, r: &TradeRequest) -> Result<TradeResult> {
        let a = r.trade_action;
        if (a == TRADE_ACTION_DEAL || a == TRADE_ACTION_PENDING) && r.volume == 0 {
            return Err(Error::validation(format!("volume must be > 0 for trade_action={a}, got {}", r.volume)));
        }
        if a == TRADE_ACTION_DEAL && r.price_order <= 0.0 { debug!("market order with price_order=0; server will use current price"); }
        if a == TRADE_ACTION_PENDING && r.price_order <= 0.0 {
            return Err(Error::validation(format!("price_order must be > 0 for pending orders, got {}", r.price_order)));
        }
        if a == TRADE_ACTION_MODIFY && r.order == 0 { return Err(Error::validation("order ticket must be > 0 for MODIFY action, got 0")); }
        if a == TRADE_ACTION_REMOVE && r.order == 0 { return Err(Error::validation("order ticket must be > 0 for REMOVE action, got 0")); }
        if a == TRADE_ACTION_SLTP && r.position_id == 0 { return Err(Error::validation("position_id must be > 0 for SLTP action, got 0")); }
        if a == TRADE_ACTION_CLOSE_BY && (r.position_id == 0 || r.position_by == 0) {
            return Err(Error::validation(format!(
                "position_id and position_by must be > 0 for CLOSE_BY, got position_id={}, position_by={}",
                r.position_id, r.position_by
            )));
        }
        if r.price_sl < 0.0 { return Err(Error::validation(format!("price_sl must be >= 0, got {}", r.price_sl))); }
        if r.price_tp < 0.0 { return Err(Error::validation(format!("price_tp must be >= 0, got {}", r.price_tp))); }
        let mut req = r.clone();
        if req.action_id == 0 { req.action_id = self.next_action_id(); }
        let r = &req;
        let payload = Self::build_trade_request_payload(r)?;
        // Register for the asynchronous result *before* sending so an early push cannot be missed.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        self.st().trade_waiters.insert(r.action_id, tx);
        let res = match self.transport().send_command(CMD_TRADE_REQUEST, &payload).await {
            Ok(res) => res,
            Err(e) => { self.st().trade_waiters.remove(&r.action_id); return Err(e); }
        };
        let full_len = get_series_size(TRADE_RESPONSE_SCHEMA)?;
        // Servers that answer cmd=12 with the full result (pymt5 layout) or with a non-zero status are done now.
        let ack = res.body.get(0..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        if res.code != 0 || res.body.len() >= full_len || ack != Some(0) {
            self.st().trade_waiters.remove(&r.action_id);
            if res.code != 0 {
                warn!("trade_request {} rejected by server: code={}", r.symbol, res.code);
                return Ok(TradeResult { retcode: -2, description: format!("command rejected by server: code={}", res.code), success: false, request_id: r.action_id as u64, ..Default::default() });
            }
            return self.parse_trade_response(&res.body, &r.symbol, r.trade_action, r.volume);
        }
        // Build 6182 behaviour: cmd=12 returns a 4-byte ack (0 = accepted); the outcome arrives as one or more
        // cmd=19 pushes (e.g. 10002 accepted, then 10008 placed / 10009 done / a rejection code). Wait for the final one.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs_f64(self.config().timeout);
        let mut last: Option<Record> = None;
        let outcome = loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(push)) => {
                    let retcode = push.get_record("result").map(|x| x.get_u64("retcode") as u32).unwrap_or(0);
                    debug!("trade result push action_id={} retcode={retcode}", r.action_id);
                    let intermediate = is_intermediate_retcode(retcode);
                    last = Some(push);
                    if !intermediate { break Ok(()); }
                }
                Ok(None) => break Err(Error::session("trade result channel closed")),
                Err(_) => break Err(Error::Timeout(format!(
                    "no final trade result push for action_id={} within {}s (last status: {}); the request was accepted - check positions/orders before retrying",
                    r.action_id,
                    self.config().timeout,
                    last.as_ref().and_then(|p| p.get_record("result")).map(|x| retcode_text(x.get_u64("retcode") as u32)).unwrap_or_else(|| "none".into())
                ))),
            }
        };
        self.st().trade_waiters.remove(&r.action_id);
        outcome?;
        let tr = Self::trade_result_from_push(&last.unwrap_or_default(), r.action_id);
        info!("trade_request {} action={} vol={} -> {tr}", r.symbol, r.trade_action, r.volume);
        Ok(tr)
    }

    /// Serialise a trade request (cmd=12) - 248 bytes, exactly as the Web Terminal JS (`getTradeRequest`) does.
    ///
    /// pymt5's `TRADE_REQUEST_SCHEMA` (236 bytes: deviation u64, no top/bottom) is *wrong* for this layout and
    /// shifts every field after `price_tp`; the server then silently ignores e.g. `position_id`.
    pub(crate) fn build_trade_request_payload(r: &TradeRequest) -> Result<Vec<u8>> {
        let u32f = |v: i64| SerField::new(PropType::U32, v, None);
        SeriesCodec::serialize(&[
            SerField::u32(r.action_id),
            u32f(r.trade_action),
            SerField::fixed_string(&trunc(&r.symbol, 32), 64),
            SerField::u64(r.volume),
            SerField::u32(r.digits),
            SerField::u64(r.order),
            u32f(r.trade_type),
            u32f(r.type_filling),
            u32f(r.type_time),
            u32f(r.type_flags),
            u32f(r.type_reason),
            SerField::f64(r.price_order),
            SerField::f64(r.price_trigger),
            SerField::f64(r.price_sl),
            SerField::f64(r.price_tp),
            SerField::new(PropType::U32, r.deviation as i64, None),
            SerField::f64(r.price_top),
            SerField::f64(r.price_bottom),
            SerField::fixed_string(&trunc(&r.comment, 32), 64),
            SerField::u64(r.position_id),
            SerField::u64(r.position_by),
            u32f(r.time_expiration),
        ])
    }

    /// Next non-zero request id used to match the cmd=19 result push with its cmd=12 request.
    pub(crate) fn next_action_id(&self) -> u32 {
        let mut st = self.st();
        let id = st.next_action_id.max(1);
        st.next_action_id = id.wrapping_add(1).max(1);
        id
    }

    fn trade_result_from_push(push: &Record, action_id: u32) -> TradeResult {
        let res = push.get_record("result").cloned().unwrap_or_default();
        let retcode = res.get_u64("retcode") as u32;
        TradeResult {
            retcode: retcode as i64,
            description: retcode_text(retcode),
            success: matches!(retcode, 0 | TRADE_RETCODE_DONE | TRADE_RETCODE_DONE_PARTIAL | TRADE_RETCODE_PLACED),
            deal: res.get_u64("deal"),
            order: res.get_u64("order"),
            volume: res.get_u64("volume"),
            price: res.get_f64("price"),
            bid: res.get_f64("bid"),
            ask: res.get_f64("ask"),
            comment: res.get_str("comment").trim_matches('\0').to_string(),
            request_id: action_id as u64,
        }
    }

    /// Official-style `order_send()` over a MetaTrader5 request dict.
    ///
    /// Keys: `action` (`TRADE_ACTION_DEAL`=1, `PENDING`=5, `SLTP`=6, `MODIFY`=7, `REMOVE`=8, `CLOSE_BY`=10),
    /// `symbol`, `volume` (lots), `type` (order type), `price`, `stoplimit`, `sl`, `tp`, `deviation`, `comment`,
    /// `position`, `position_by`, `order`, `type_filling`, `type_time`, `expiration`, `magic`.
    ///
    /// The request is translated to what the Web Terminal sends (see docs/trading.md): e.g. a DEAL becomes the
    /// execution-mode specific action/filling of the symbol, and SLTP/MODIFY/REMOVE/close-by look up the existing
    /// position or order first. Use [`Mt5Client::trade_request`] to send raw fields.
    pub async fn order_send(&self, request: &Record) -> Result<TradeResult> {
        use crate::order_helpers::{CloseOptions, MarketOrderOptions, ModifyOrderOptions, PendingOrderOptions};
        let symbol = request.get_str("symbol").to_string();
        let order_type = request.get_i64("type");
        let volume = request.get_f64("volume");
        let price = request.get_f64("price");
        let filling = request.contains_key("type_filling").then(|| request.get_i64("type_filling"));
        let opt = |k: &str| request.contains_key(k).then(|| request.get_f64(k));
        match request.get_i64("action") {
            TRADE_ACTION_DEAL => {
                let position = request.get_u64("position");
                if position != 0 {
                    let vol = (volume > 0.0).then_some(volume);
                    self.close_position(position, vol, &CloseOptions {
                        deviation: request.get_u64("deviation"), comment: request.get_str("comment").to_string(), filling, magic: request.get_i64("magic"),
                    }).await
                } else {
                    let o = MarketOrderOptions {
                        sl: request.get_f64("sl"), tp: request.get_f64("tp"), price: (price > 0.0).then_some(price), deviation: request.get_u64("deviation"),
                        comment: request.get_str("comment").to_string(), filling, digits: None, magic: request.get_i64("magic"),
                    };
                    match order_type {
                        ORDER_TYPE_BUY => self.buy_market(&symbol, volume, &o).await,
                        ORDER_TYPE_SELL => self.sell_market(&symbol, volume, &o).await,
                        t => Err(Error::validation(format!("DEAL needs order type BUY(0) or SELL(1), got {t}"))),
                    }
                }
            }
            TRADE_ACTION_PENDING => {
                let (order_price, trigger) = if order_type == ORDER_TYPE_BUY_STOP_LIMIT || order_type == ORDER_TYPE_SELL_STOP_LIMIT { (price, request.get_f64("stoplimit")) } else { (price, 0.0) };
                if order_side(order_type).is_none() || order_type <= ORDER_TYPE_SELL { return Err(Error::validation(format!("PENDING needs a pending order type (2..7), got {order_type}"))); }
                let o = PendingOrderOptions {
                    sl: request.get_f64("sl"), tp: request.get_f64("tp"), comment: request.get_str("comment").to_string(), filling,
                    time_type: if request.contains_key("type_time") { request.get_i64("type_time") } else { ORDER_TIME_GTC },
                    expiration: coerce_value_timestamp(request.get("expiration")), digits: None, magic: request.get_i64("magic"),
                };
                self.open_pending_typed(&symbol, volume, order_type, order_price, trigger, &o).await
            }
            TRADE_ACTION_SLTP => self.modify_position_sltp(request.get_u64("position"), opt("sl"), opt("tp")).await,
            TRADE_ACTION_MODIFY => {
                let stop_limit = order_type == ORDER_TYPE_BUY_STOP_LIMIT || order_type == ORDER_TYPE_SELL_STOP_LIMIT;
                self.modify_pending_order(request.get_u64("order"), &ModifyOrderOptions {
                    price: opt("price"),
                    trigger: if stop_limit { opt("stoplimit") } else { None },
                    sl: opt("sl"), tp: opt("tp"),
                    time_type: request.contains_key("type_time").then(|| request.get_i64("type_time")),
                    expiration: request.contains_key("expiration").then(|| coerce_value_timestamp(request.get("expiration"))),
                    comment: request.contains_key("comment").then(|| request.get_str("comment").to_string()),
                }).await
            }
            TRADE_ACTION_REMOVE => self.cancel_pending_order(request.get_u64("order")).await,
            TRADE_ACTION_CLOSE_BY => self.close_position_by(request.get_u64("position"), request.get_u64("position_by")).await,
            a => Err(Error::validation(format!("unsupported trade action: {a}"))),
        }
    }

    /// Local pre-flight validation (not a server RPC). Returns `retcode`, balances, margin and `comment`.
    pub async fn order_check(&self, request: &Record) -> Result<Record> {
        match self.order_check_inner(request).await {
            Err(e @ (Error::Protocol(_) | Error::Validation(_))) => {
                let message = format!("order_check() failed: {e}");
                self.st().last_error = (-99, message.clone());
                let a = self.get_account().await?;
                let balance = a.get_f64("balance");
                let equity = if a.contains_key("equity") { a.get_f64("equity") } else { balance };
                let margin = a.get_f64("margin");
                let margin_free = if a.get_f64("margin_free") != 0.0 { a.get_f64("margin_free") } else { equity - margin };
                Ok(Record::new()
                    .with("retcode", -99i64)
                    .with("balance", balance)
                    .with("equity", equity)
                    .with("profit", a.get_f64("profit"))
                    .with("margin", margin)
                    .with("margin_free", margin_free)
                    .with("margin_level", a.get_f64("margin_level"))
                    .with("comment", message)
                    .with("request", request.clone()))
            }
            other => other,
        }
    }

    async fn order_check_inner(&self, request: &Record) -> Result<Record> {
        let normalized = Self::normalize_order_request(request);
        let (mut retcode, mut comment) = self.validate_order_check_request(&normalized).await?;
        let account = self.get_account().await?;
        // the account record has no margin figures: compute them from the live positions
        let am = self.account_margin_from(&account).await?;
        let balance = account.get_f64("balance");
        let equity = am.get_f64("equity");
        let profit = am.get_f64("profit");
        let margin = am.get_f64("margin");
        let ok_codes = [0i64, TRADE_RETCODE_PLACED as i64, TRADE_RETCODE_DONE as i64, TRADE_RETCODE_DONE_PARTIAL as i64];
        let estimate = if ok_codes.contains(&retcode) { self.estimate_order_margin(&normalized).await? } else { Some(0.0) };
        let margin_known = estimate.is_some();
        let additional = estimate.unwrap_or(0.0);
        let margin_post = margin + additional;
        let margin_free = equity - margin_post;
        let margin_level = if margin_post <= 0.0 { 0.0 } else { (equity / margin_post) * 100.0 };
        if ok_codes.contains(&retcode) && margin_free < 0.0 {
            retcode = TRADE_RETCODE_NO_MONEY as i64;
            comment = retcode_description(TRADE_RETCODE_NO_MONEY).unwrap_or("Not enough money").to_string();
        }
        if retcode == 0 { self.clear_last_error(); } else { self.st().last_error = (retcode, comment.clone()); }
        Ok(Record::new()
            .with("retcode", retcode)
            .with("balance", balance)
            .with("equity", equity)
            .with("profit", profit)
            .with("margin", margin_post)
            .with("margin_free", margin_free)
            .with("margin_level", margin_level)
            .with("comment", comment)
            // false when the margin could not be computed (e.g. no conversion rate); margin/margin_free
            // then only reflect the existing account margin, so a "passing" check is not conclusive.
            .with("margin_known", margin_known)
            .with("request", normalized))
    }

    fn normalize_order_request(request: &Record) -> Record {
        let order_type = request.get_i64("type");
        let price = request.get_f64("price");
        let stoplimit = request.get_f64("stoplimit");
        let (price_order, price_trigger) =
            if order_type == ORDER_TYPE_BUY_STOP_LIMIT || order_type == ORDER_TYPE_SELL_STOP_LIMIT { (price, stoplimit) } else { (price, 0.0) };
        Record::new()
            .with("action", request.get_i64("action"))
            .with("symbol", request.get_str("symbol"))
            .with("volume", request.get_f64("volume"))
            .with("type", order_type)
            .with("price", price)
            .with("price_order", price_order)
            .with("price_trigger", price_trigger)
            .with("sl", request.get_f64("sl"))
            .with("tp", request.get_f64("tp"))
            .with("type_filling", if request.contains_key("type_filling") { request.get_i64("type_filling") } else { ORDER_FILLING_FOK })
            .with("type_time", if request.contains_key("type_time") { request.get_i64("type_time") } else { ORDER_TIME_GTC })
            .with("expiration", coerce_value_timestamp(request.get("expiration")))
            .with("position", request.get_i64("position"))
            .with("position_by", request.get_i64("position_by"))
            .with("order", request.get_i64("order"))
            .with("deviation", request.get_i64("deviation"))
            .with("comment", request.get_str("comment"))
    }

    async fn validate_deal_or_pending(&self, request: &Record, symbol_info: Option<&Record>) -> Result<Option<(i64, String)>> {
        let desc = |c: u32| retcode_description(c).unwrap_or("").to_string();
        let symbol = request.get_str("symbol");
        let order_type = request.get_i64("type");
        let volume = request.get_f64("volume");
        let action = request.get_i64("action");
        if symbol.is_empty() { return Ok(Some((TRADE_RETCODE_INVALID as i64, desc(TRADE_RETCODE_INVALID)))); }
        let Some(info) = symbol_info else { return Ok(Some((TRADE_RETCODE_INVALID as i64, format!("symbol not found: {symbol}")))) };
        if volume <= 0.0 { return Ok(Some((TRADE_RETCODE_INVALID_VOLUME as i64, desc(TRADE_RETCODE_INVALID_VOLUME)))); }
        let side = order_side(order_type);
        if side.is_none() { return Ok(Some((TRADE_RETCODE_INVALID_ORDER as i64, desc(TRADE_RETCODE_INVALID_ORDER)))); }
        let trade_mode = info.get_i64("trade_mode");
        if trade_mode == 0 { return Ok(Some((TRADE_RETCODE_TRADE_DISABLED as i64, desc(TRADE_RETCODE_TRADE_DISABLED)))); }
        if trade_mode == 1 && side == Some(false) { return Ok(Some((TRADE_RETCODE_INVALID_ORDER as i64, "symbol is long-only".into()))); }
        if trade_mode == 2 && side == Some(true) { return Ok(Some((TRADE_RETCODE_INVALID_ORDER as i64, "symbol is short-only".into()))); }
        if trade_mode == 3 { return Ok(Some((TRADE_RETCODE_TRADE_DISABLED as i64, "symbol is close-only".into()))); }
        if let Some(msg) = validate_requested_volume(&symbol_info_volumes_in_lots(info), volume) { return Ok(Some((TRADE_RETCODE_INVALID_VOLUME as i64, msg))); }
        let expiration = request.get_i64("expiration");
        let type_time = if request.contains_key("type_time") { request.get_i64("type_time") } else { ORDER_TIME_GTC };
        if (type_time == ORDER_TIME_SPECIFIED || type_time == ORDER_TIME_SPECIFIED_DAY) && expiration <= 0 {
            return Ok(Some((TRADE_RETCODE_INVALID_EXPIRATION as i64, desc(TRADE_RETCODE_INVALID_EXPIRATION))));
        }
        let fill_flags = info.get_i64("filling_mode");
        let type_filling = if request.contains_key("type_filling") { request.get_i64("type_filling") } else { ORDER_FILLING_FOK };
        if fill_flags != 0 && !matches!(type_filling, 0..=2) {
            return Ok(Some((TRADE_RETCODE_INVALID_FILL as i64, desc(TRADE_RETCODE_INVALID_FILL))));
        }
        let price_check = self.resolve_order_check_price(request, Some(info));
        if price_check <= 0.0 { return Ok(Some((TRADE_RETCODE_INVALID_PRICE as i64, desc(TRADE_RETCODE_INVALID_PRICE)))); }
        if action == TRADE_ACTION_PENDING && request.get_f64("price_order") <= 0.0 {
            return Ok(Some((TRADE_RETCODE_INVALID_PRICE as i64, desc(TRADE_RETCODE_INVALID_PRICE))));
        }
        if let Some(msg) = validate_requested_stops(info, request, price_check) { return Ok(Some((TRADE_RETCODE_INVALID_STOPS as i64, msg))); }
        Ok(None)
    }

    async fn validate_order_check_request(&self, request: &Record) -> Result<(i64, String)> {
        let action = request.get_i64("action");
        let symbol = request.get_str("symbol");
        let info = if symbol.is_empty() { None } else { self.symbol_info(symbol).await? };
        let invalid = |m: &str| Ok((TRADE_RETCODE_INVALID as i64, m.to_string()));
        match action {
            TRADE_ACTION_DEAL | TRADE_ACTION_PENDING => {
                if let Some(r) = self.validate_deal_or_pending(request, info.as_ref()).await? { return Ok(r); }
            }
            TRADE_ACTION_SLTP => if request.get_i64("position") <= 0 { return invalid("position is required for SLTP modification") },
            TRADE_ACTION_MODIFY => if request.get_i64("order") <= 0 { return invalid("order is required for order modification") },
            TRADE_ACTION_REMOVE => if request.get_i64("order") <= 0 { return invalid("order is required for order removal") },
            TRADE_ACTION_CLOSE_BY => {
                if request.get_i64("position") <= 0 || request.get_i64("position_by") <= 0 {
                    return invalid("position and position_by are required for close_by");
                }
            }
            _ => return invalid(retcode_description(TRADE_RETCODE_INVALID).unwrap_or("")),
        }
        Ok((0, retcode_description(0).unwrap_or("OK").to_string()))
    }

    /// `Some(0.0)` when no margin applies, `None` when it could not be computed.
    async fn estimate_order_margin(&self, request: &Record) -> Result<Option<f64>> {
        let action = request.get_i64("action");
        if action != TRADE_ACTION_DEAL && action != TRADE_ACTION_PENDING { return Ok(Some(0.0)); }
        let info = self.symbol_info(request.get_str("symbol")).await?;
        let price = self.resolve_order_check_price(request, info.as_ref());
        if price <= 0.0 { return Ok(None); }
        self.order_calc_margin(request.get_i64("type"), request.get_str("symbol"), request.get_f64("volume"), price).await
    }

    fn resolve_order_check_price(&self, request: &Record, symbol_info: Option<&Record>) -> f64 {
        if symbol_info.is_none() { return 0.0; }
        let action = request.get_i64("action");
        let order_type = request.get_i64("type");
        if action == TRADE_ACTION_PENDING {
            return request.get_f64("price_order"); // stop-limit too: price_order is the stop (activation) price
        }
        let mut price = request.get_f64("price_order");
        if price == 0.0 { price = request.get_f64("price"); }
        if price > 0.0 { return price; }
        if let Some(tick) = self.symbol_info_tick(request.get_str("symbol")) {
            let (ask, bid) = (tick.get_f64("ask"), tick.get_f64("bid"));
            return if order_side(order_type) == Some(true) { if ask != 0.0 { ask } else { bid } } else if bid != 0.0 { bid } else { ask };
        }
        0.0
    }

    fn parse_trade_response(&self, body: &[u8], symbol: &str, trade_action: i64, volume: u64) -> Result<TradeResult> {
        if body.is_empty() {
            warn!("trade_request {symbol} action={trade_action} vol={volume} -> empty response");
            return Ok(TradeResult { retcode: -1, description: "Empty response".into(), success: false, ..Default::default() });
        }
        let retcode = body.get(0..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| Error::protocol("trade response too short"))?;
        let description = retcode_text(retcode);
        let success = matches!(retcode, 0 | TRADE_RETCODE_DONE | TRADE_RETCODE_DONE_PARTIAL | TRADE_RETCODE_PLACED);
        let base = TradeResult { retcode: retcode as i64, description, success, ..Default::default() };
        let tr = if body.len() >= get_series_size(TRADE_RESPONSE_SCHEMA)? {
            match SeriesCodec::parse(body, TRADE_RESPONSE_SCHEMA, 0) {
                Ok(v) => TradeResult {
                    retcode: v[0].as_i64().unwrap_or(retcode as i64),
                    deal: v[1].as_u64().unwrap_or(0),
                    order: v[2].as_u64().unwrap_or(0),
                    volume: v[3].as_u64().unwrap_or(0),
                    price: v[4].as_f64().unwrap_or(0.0),
                    bid: v[5].as_f64().unwrap_or(0.0),
                    ask: v[6].as_f64().unwrap_or(0.0),
                    comment: v[7].as_str().unwrap_or("").trim_matches('\0').to_string(),
                    request_id: v[8].as_u64().unwrap_or(0),
                    ..base
                },
                Err(e) => { debug!("trade response extended parse failed: {e}, using retcode only"); base }
            }
        } else {
            base
        };
        info!("trade_request {symbol} action={trade_action} vol={volume} -> {tr}");
        Ok(tr)
    }
}

