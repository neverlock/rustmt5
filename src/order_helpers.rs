//! Order convenience methods.
//!
//! Unlike pymt5 (which always sent `trade_action = DEAL` with FOK filling) these follow what the
//! Web Terminal JS does, which is what real servers accept:
//!
//! * market open/close pick `trade_action` and filling from the symbol's **execution mode**
//!   (`trade_exemode`: 0 request, 1 instant, 2 market, 3 exchange) and `trade_fill_flags`;
//! * new orders carry `type_flags = 2`, closes `type_flags = 3` + the position ticket;
//! * cancel / modify / SLTP first look up the existing order or position and resend its volume,
//!   symbol, type and prices.

use std::time::Duration;

use tracing::{debug, warn};

use crate::client::Mt5Client;
use crate::constants::*;
use crate::error::{Error, Result};
use crate::protocol::Record;
use crate::trading::TradeRequest;
use crate::types::TradeResult;
use crate::validation::{validate_symbol_name, validate_volume};

/// Symbol `trade_fill_flags` bits.
const FILL_FOK: i64 = 1;
const FILL_IOC: i64 = 2;

/// Options for market orders.
#[derive(Debug, Clone, Default)]
pub struct MarketOrderOptions {
    pub sl: f64,
    pub tp: f64,
    /// Reference price; `None` uses the cached tick (ask for buy, bid for sell).
    pub price: Option<f64>,
    pub deviation: u64,
    pub comment: String,
    /// `None` = choose from the symbol's fill flags (IOC, else FOK, else RETURN).
    pub filling: Option<i64>,
    /// `None` = from the symbol info.
    pub digits: Option<u32>,
    pub magic: i64,
}

/// Options for pending (limit/stop/stop-limit) orders.
#[derive(Debug, Clone)]
pub struct PendingOrderOptions {
    pub sl: f64,
    pub tp: f64,
    pub comment: String,
    /// `None` = RETURN (2), the only filling servers accept for pending orders in market execution.
    pub filling: Option<i64>,
    pub time_type: i64,
    pub expiration: i64,
    pub digits: Option<u32>,
    pub magic: i64,
}
impl Default for PendingOrderOptions {
    fn default() -> Self {
        Self { sl: 0.0, tp: 0.0, comment: String::new(), filling: None, time_type: ORDER_TIME_GTC, expiration: 0, digits: None, magic: 0 }
    }
}

/// Options for [`Mt5Client::close_position`].
#[derive(Debug, Clone, Default)]
pub struct CloseOptions {
    pub deviation: u64,
    pub comment: String,
    /// `None` = choose from the symbol's fill flags.
    pub filling: Option<i64>,
    pub magic: i64,
}

/// Fields to change in [`Mt5Client::modify_pending_order`]; `None` keeps the current value.
#[derive(Debug, Clone, Default)]
pub struct ModifyOrderOptions {
    pub price: Option<f64>,
    /// Trigger price of stop-limit orders.
    pub trigger: Option<f64>,
    pub sl: Option<f64>,
    pub tp: Option<f64>,
    pub time_type: Option<i64>,
    pub expiration: Option<i64>,
    pub comment: Option<String>,
}

/// Round to the symbol's tick size and digits (what the Web Terminal does before sending prices).
pub fn normalize_price(price: f64, tick_size: f64, digits: u32) -> f64 {
    if price == 0.0 { return 0.0; }
    let p = if tick_size > 0.0 { (price / tick_size).round() * tick_size } else { price };
    let scale = 10f64.powi(digits as i32);
    (p * scale).round() / scale
}

/// How a symbol wants market deals to be sent.
struct Exec {
    exemode: i64,
    fill_flags: i64,
    instant_volume: u64,
    tick_size: f64,
    digits: u32,
}

impl Exec {
    fn auto_filling(&self) -> i64 {
        if self.fill_flags & FILL_IOC != 0 { 1 } else if self.fill_flags & FILL_FOK != 0 { 0 } else { 2 }
    }
}

impl Mt5Client {
    async fn exec_params(&self, symbol: &str, digits: Option<u32>) -> Result<Exec> {
        validate_symbol_name(symbol)?;
        let info = self.symbol_info(symbol).await?.ok_or_else(|| Error::validation(format!("symbol not found: {symbol}")))?;
        let trade = info.get_record("trade").cloned().unwrap_or_default();
        Ok(Exec {
            exemode: trade.get_i64("trade_exemode"),
            fill_flags: trade.get_i64("trade_fill_flags"),
            instant_volume: trade.get_u64("trade_instant_volume"),
            tick_size: info.get_f64("tick_size"),
            digits: digits.unwrap_or_else(|| info.get_u64("digits") as u32),
        })
    }

    fn reference_price(&self, symbol: &str, is_buy: bool) -> f64 {
        self.symbol_info_tick(symbol).map(|t| if is_buy { t.get_f64("ask") } else { t.get_f64("bid") }).unwrap_or(0.0)
    }

    /// Fill `trade_action`, `type_filling` and `price_order` for a market deal according to the execution mode.
    fn apply_execution_mode(req: &mut TradeRequest, ex: &Exec, price: f64, filling: Option<i64>) {
        let mut mode = ex.exemode;
        if mode == 1 {
            req.type_filling = filling.unwrap_or(0);
            if ex.instant_volume == 0 || req.volume <= ex.instant_volume {
                req.trade_action = TRADE_ACTION_INSTANT;
                req.price_order = price;
                return;
            }
            mode = 0; // too large for instant execution: fall back to request execution
        }
        match mode {
            2 => {
                req.trade_action = TRADE_ACTION_MARKET;
                req.type_filling = filling.unwrap_or_else(|| ex.auto_filling());
            }
            3 => {
                req.trade_action = TRADE_ACTION_EXCHANGE;
                req.type_filling = filling.unwrap_or(2);
            }
            _ => {
                if price > 0.0 { req.price_order = price; req.trade_action = TRADE_ACTION_DEAL; } else { req.trade_action = 0; }
            }
        }
    }

    async fn open_market(&self, symbol: &str, volume: f64, trade_type: i64, o: &MarketOrderOptions) -> Result<TradeResult> {
        validate_volume(volume)?;
        let ex = self.exec_params(symbol, o.digits).await?;
        let is_buy = trade_type == ORDER_TYPE_BUY;
        let price = normalize_price(o.price.unwrap_or_else(|| self.reference_price(symbol, is_buy)), ex.tick_size, ex.digits);
        let mut req = TradeRequest {
            symbol: symbol.to_string(),
            volume: Self::volume_to_lots(volume, VOLUME_PRECISION).max(0) as u64,
            digits: ex.digits,
            trade_type,
            type_flags: 2,
            price_sl: normalize_price(o.sl, ex.tick_size, ex.digits),
            price_tp: normalize_price(o.tp, ex.tick_size, ex.digits),
            deviation: o.deviation,
            comment: o.comment.clone(),
            type_reason: o.magic,
            ..Default::default()
        };
        Self::apply_execution_mode(&mut req, &ex, price, o.filling);
        if ex.exemode == 3 { req.price_order = 0.0; }
        self.trade_request(&req).await
    }

    pub(crate) async fn open_pending_typed(&self, symbol: &str, volume: f64, trade_type: i64, price: f64, trigger: f64, o: &PendingOrderOptions) -> Result<TradeResult> {
        self.open_pending(symbol, volume, trade_type, price, trigger, o).await
    }

    async fn open_pending(&self, symbol: &str, volume: f64, trade_type: i64, price: f64, trigger: f64, o: &PendingOrderOptions) -> Result<TradeResult> {
        validate_volume(volume)?;
        let ex = self.exec_params(symbol, o.digits).await?;
        self.trade_request(&TradeRequest {
            trade_action: TRADE_ACTION_PENDING,
            symbol: symbol.to_string(),
            volume: Self::volume_to_lots(volume, VOLUME_PRECISION).max(0) as u64,
            digits: ex.digits,
            trade_type,
            type_filling: o.filling.unwrap_or(2),
            type_time: o.time_type,
            type_flags: 2,
            type_reason: o.magic,
            price_order: normalize_price(price, ex.tick_size, ex.digits),
            price_trigger: normalize_price(trigger, ex.tick_size, ex.digits),
            price_sl: normalize_price(o.sl, ex.tick_size, ex.digits),
            price_tp: normalize_price(o.tp, ex.tick_size, ex.digits),
            comment: o.comment.clone(),
            time_expiration: o.expiration,
            ..Default::default()
        })
        .await
    }

    pub async fn buy_market(&self, symbol: &str, volume: f64, o: &MarketOrderOptions) -> Result<TradeResult> { self.open_market(symbol, volume, ORDER_TYPE_BUY, o).await }
    pub async fn sell_market(&self, symbol: &str, volume: f64, o: &MarketOrderOptions) -> Result<TradeResult> { self.open_market(symbol, volume, ORDER_TYPE_SELL, o).await }
    pub async fn buy_limit(&self, symbol: &str, volume: f64, price: f64, o: &PendingOrderOptions) -> Result<TradeResult> { self.open_pending(symbol, volume, ORDER_TYPE_BUY_LIMIT, price, 0.0, o).await }
    pub async fn sell_limit(&self, symbol: &str, volume: f64, price: f64, o: &PendingOrderOptions) -> Result<TradeResult> { self.open_pending(symbol, volume, ORDER_TYPE_SELL_LIMIT, price, 0.0, o).await }
    pub async fn buy_stop(&self, symbol: &str, volume: f64, price: f64, o: &PendingOrderOptions) -> Result<TradeResult> { self.open_pending(symbol, volume, ORDER_TYPE_BUY_STOP, price, 0.0, o).await }
    pub async fn sell_stop(&self, symbol: &str, volume: f64, price: f64, o: &PendingOrderOptions) -> Result<TradeResult> { self.open_pending(symbol, volume, ORDER_TYPE_SELL_STOP, price, 0.0, o).await }

    /// Buy stop-limit: `price` is the stop trigger, `stop_limit_price` the limit placed after the trigger.
    pub async fn buy_stop_limit(&self, symbol: &str, volume: f64, price: f64, stop_limit_price: f64, o: &PendingOrderOptions) -> Result<TradeResult> {
        self.open_pending(symbol, volume, ORDER_TYPE_BUY_STOP_LIMIT, stop_limit_price, price, o).await
    }

    pub async fn sell_stop_limit(&self, symbol: &str, volume: f64, price: f64, stop_limit_price: f64, o: &PendingOrderOptions) -> Result<TradeResult> {
        self.open_pending(symbol, volume, ORDER_TYPE_SELL_STOP_LIMIT, stop_limit_price, price, o).await
    }

    /// Positions and orders reported by the server lag a few seconds behind a trade: look a few times.
    async fn find_position(&self, position_id: u64) -> Result<Record> {
        for attempt in 0..4 {
            if let Some(p) = self.get_positions().await?.into_iter().find(|p| p.get_u64("position_id") == position_id) { return Ok(p); }
            if attempt < 3 { tokio::time::sleep(Duration::from_millis(1000)).await; }
        }
        Err(Error::validation(format!("position {position_id} not found")))
    }

    async fn find_order(&self, order: u64) -> Result<Record> {
        for attempt in 0..4 {
            if let Some(o) = self.get_orders().await?.into_iter().find(|o| o.get_u64("trade_order") == order) { return Ok(o); }
            if attempt < 3 { tokio::time::sleep(Duration::from_millis(1000)).await; }
        }
        Err(Error::validation(format!("pending order {order} not found")))
    }

    /// Close a position with an opposite deal. `volume = None` closes it completely.
    pub async fn close_position(&self, position_id: u64, volume: Option<f64>, o: &CloseOptions) -> Result<TradeResult> {
        let pos = self.find_position(position_id).await?;
        let symbol = pos.get_str("trade_symbol").to_string();
        let ex = self.exec_params(&symbol, None).await?;
        let is_long = pos.get_i64("trade_action") == POSITION_TYPE_BUY;
        let wire_volume = match volume {
            Some(v) => { validate_volume(v)?; Self::volume_to_lots(v, VOLUME_PRECISION).max(0) as u64 }
            None => pos.get_u64("trade_volume"),
        };
        let digits = { let d = pos.get_u64("digits") as u32; if d > 0 { d } else { ex.digits } };
        let mut req = TradeRequest {
            symbol,
            volume: wire_volume,
            digits,
            trade_type: if is_long { ORDER_TYPE_SELL } else { ORDER_TYPE_BUY },
            type_flags: 3,
            position_id,
            deviation: o.deviation,
            comment: o.comment.clone(),
            type_reason: o.magic,
            ..Default::default()
        };
        let price = pos.get_f64("price_close");
        Self::apply_execution_mode(&mut req, &ex, price, o.filling);
        if ex.exemode != 3 { req.price_order = price; } else { req.price_order = 0.0; }
        debug!("close_position {position_id}: action={} filling={}", req.trade_action, req.type_filling);
        self.trade_request(&req).await
    }

    /// Close `position_id` against the opposite position `position_by` (same symbol, opposite direction).
    pub async fn close_position_by(&self, position_id: u64, position_by: u64) -> Result<TradeResult> {
        let pos = self.find_position(position_id).await?;
        let other = self.find_position(position_by).await?;
        if pos.get_str("trade_symbol") != other.get_str("trade_symbol") || pos.get_i64("trade_action") == other.get_i64("trade_action") {
            return Err(Error::validation("close-by needs two opposite positions of the same symbol"));
        }
        self.trade_request(&TradeRequest {
            trade_action: TRADE_ACTION_CLOSE_BY,
            symbol: pos.get_str("trade_symbol").to_string(),
            volume: pos.get_u64("trade_volume").min(other.get_u64("trade_volume")),
            digits: pos.get_u64("digits") as u32,
            trade_type: if pos.get_i64("trade_action") == POSITION_TYPE_BUY { ORDER_TYPE_SELL } else { ORDER_TYPE_BUY },
            type_flags: 3,
            price_order: pos.get_f64("price_close"),
            position_id,
            position_by,
            ..Default::default()
        })
        .await
    }

    /// Change SL/TP of a position; `None` keeps the current value, `Some(0.0)` removes it.
    pub async fn modify_position_sltp(&self, position_id: u64, sl: Option<f64>, tp: Option<f64>) -> Result<TradeResult> {
        let pos = self.find_position(position_id).await?;
        let symbol = pos.get_str("trade_symbol").to_string();
        let ex = self.exec_params(&symbol, None).await?;
        self.trade_request(&TradeRequest {
            trade_action: TRADE_ACTION_SLTP,
            symbol,
            volume: pos.get_u64("trade_volume"),
            digits: ex.digits,
            position_id,
            price_sl: normalize_price(sl.unwrap_or_else(|| pos.get_f64("sl")), ex.tick_size, ex.digits),
            price_tp: normalize_price(tp.unwrap_or_else(|| pos.get_f64("tp")), ex.tick_size, ex.digits),
            ..Default::default()
        })
        .await
    }

    /// Modify a pending order (all unspecified fields keep their current value).
    pub async fn modify_pending_order(&self, order: u64, m: &ModifyOrderOptions) -> Result<TradeResult> {
        let o = self.find_order(order).await?;
        let symbol = o.get_str("trade_symbol").to_string();
        let ex = self.exec_params(&symbol, None).await?;
        self.trade_request(&TradeRequest {
            trade_action: TRADE_ACTION_MODIFY,
            symbol,
            volume: o.get_u64("volume_initial"),
            digits: ex.digits,
            order,
            trade_type: o.get_i64("order_type"),
            type_filling: o.get_i64("type_filling"),
            type_time: m.time_type.unwrap_or_else(|| o.get_i64("type_time")),
            price_order: normalize_price(m.price.unwrap_or_else(|| o.get_f64("price_order")), ex.tick_size, ex.digits),
            price_trigger: normalize_price(m.trigger.unwrap_or_else(|| o.get_f64("price_trigger")), ex.tick_size, ex.digits),
            price_sl: normalize_price(m.sl.unwrap_or_else(|| o.get_f64("price_sl")), ex.tick_size, ex.digits),
            price_tp: normalize_price(m.tp.unwrap_or_else(|| o.get_f64("price_tp")), ex.tick_size, ex.digits),
            time_expiration: m.expiration.unwrap_or_else(|| o.get_i64("time_expiration")),
            comment: m.comment.clone().unwrap_or_else(|| o.get_str("comment").to_string()),
            ..Default::default()
        })
        .await
    }

    /// Cancel a pending order.
    pub async fn cancel_pending_order(&self, order: u64) -> Result<TradeResult> {
        let o = self.find_order(order).await.map_err(|e| { warn!("cancel_pending_order: {e}"); e })?;
        let symbol = o.get_str("trade_symbol").to_string();
        self.trade_request(&TradeRequest {
            trade_action: TRADE_ACTION_REMOVE,
            symbol,
            volume: o.get_u64("volume_initial"),
            order,
            trade_type: o.get_i64("order_type"),
            type_time: o.get_i64("type_time"),
            price_order: o.get_f64("price_order"),
            price_trigger: o.get_f64("price_trigger"),
            price_sl: o.get_f64("price_sl"),
            price_tp: o.get_f64("price_tp"),
            time_expiration: o.get_i64("time_expiration"),
            ..Default::default()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_normalisation() {
        assert_eq!(normalize_price(0.0, 0.0, 5), 0.0);
        assert_eq!(normalize_price(1.123456789, 0.0, 5), 1.12346);
        assert_eq!(normalize_price(1.1234, 0.0005, 5), 1.1235);
        assert_eq!(normalize_price(100.0, 0.0, 2), 100.0);
    }

    fn ex(mode: i64, flags: i64) -> Exec { Exec { exemode: mode, fill_flags: flags, instant_volume: 0, tick_size: 0.0, digits: 5 } }

    #[test]
    fn action_and_filling_follow_execution_mode() {
        let mut r = TradeRequest { volume: 1, ..Default::default() };
        Mt5Client::apply_execution_mode(&mut r, &ex(2, 3), 1.1, None);
        assert_eq!((r.trade_action, r.type_filling, r.price_order), (3, 1, 0.0), "market execution: action 3, IOC preferred");
        let mut r = TradeRequest::default();
        Mt5Client::apply_execution_mode(&mut r, &ex(2, 1), 1.1, None);
        assert_eq!(r.type_filling, 0, "FOK only");
        let mut r = TradeRequest::default();
        Mt5Client::apply_execution_mode(&mut r, &ex(2, 0), 1.1, None);
        assert_eq!(r.type_filling, 2, "RETURN fallback");
        let mut r = TradeRequest::default();
        Mt5Client::apply_execution_mode(&mut r, &ex(2, 3), 1.1, Some(0));
        assert_eq!(r.type_filling, 0, "explicit filling wins");
        let mut r = TradeRequest::default();
        Mt5Client::apply_execution_mode(&mut r, &ex(0, 3), 1.1, None);
        assert_eq!((r.trade_action, r.price_order), (1, 1.1));
        let mut r = TradeRequest::default();
        Mt5Client::apply_execution_mode(&mut r, &ex(0, 3), 0.0, None);
        assert_eq!(r.trade_action, 0, "request execution without price");
        let mut r = TradeRequest { volume: 5, ..Default::default() };
        Mt5Client::apply_execution_mode(&mut r, &ex(1, 3), 1.1, None);
        assert_eq!((r.trade_action, r.type_filling, r.price_order), (2, 0, 1.1));
        let mut r = TradeRequest { volume: 5, ..Default::default() };
        let mut e = ex(1, 3);
        e.instant_volume = 2;
        Mt5Client::apply_execution_mode(&mut r, &e, 1.1, None);
        assert_eq!(r.trade_action, 1, "above the instant volume limit -> request execution");
        let mut r = TradeRequest::default();
        Mt5Client::apply_execution_mode(&mut r, &ex(3, 3), 1.1, None);
        assert_eq!((r.trade_action, r.type_filling), (4, 2));
    }
}
