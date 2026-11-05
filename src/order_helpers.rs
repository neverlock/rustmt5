//! Order convenience methods (port of `pymt5._order_helpers`).

use tracing::{debug, warn};

use crate::client::{is_soft_error, Mt5Client};
use crate::constants::*;
use crate::error::Result;
use crate::trading::TradeRequest;
use crate::types::TradeResult;
use crate::validation::{validate_symbol_name, validate_volume};

/// Options for market orders.
#[derive(Debug, Clone)]
pub struct MarketOrderOptions {
    pub sl: f64,
    pub tp: f64,
    pub deviation: u64,
    pub comment: String,
    pub filling: i64,
    /// `None` = look up from the symbol cache (fallback 5).
    pub digits: Option<u32>,
    pub magic: i64,
}
impl Default for MarketOrderOptions {
    fn default() -> Self { Self { sl: 0.0, tp: 0.0, deviation: 20, comment: String::new(), filling: ORDER_FILLING_FOK, digits: None, magic: 0 } }
}

/// Options for pending (limit/stop/stop-limit) orders.
#[derive(Debug, Clone)]
pub struct PendingOrderOptions {
    pub sl: f64,
    pub tp: f64,
    pub comment: String,
    pub filling: i64,
    pub time_type: i64,
    pub expiration: i64,
    pub digits: Option<u32>,
    pub magic: i64,
}
impl Default for PendingOrderOptions {
    fn default() -> Self {
        Self { sl: 0.0, tp: 0.0, comment: String::new(), filling: ORDER_FILLING_FOK, time_type: ORDER_TIME_GTC, expiration: 0, digits: None, magic: 0 }
    }
}

/// Options for [`Mt5Client::close_position`].
#[derive(Debug, Clone)]
pub struct CloseOptions {
    /// Order type used to close; `None` auto-detects the opposite of the open position.
    pub order_type: Option<i64>,
    pub deviation: u64,
    pub comment: String,
    pub filling: i64,
    pub digits: Option<u32>,
    pub magic: i64,
}
impl Default for CloseOptions {
    fn default() -> Self { Self { order_type: None, deviation: 20, comment: String::new(), filling: ORDER_FILLING_FOK, digits: None, magic: 0 } }
}

struct Place<'a> {
    trade_action: i64,
    symbol: &'a str,
    volume: f64,
    trade_type: i64,
    digits: Option<u32>,
    price: f64,
    trigger: f64,
    sl: f64,
    tp: f64,
    deviation: u64,
    comment: &'a str,
    filling: i64,
    time_type: i64,
    expiration: i64,
    magic: i64,
}

impl Mt5Client {
    async fn place_order(&self, p: Place<'_>) -> Result<TradeResult> {
        validate_symbol_name(p.symbol)?;
        if p.trade_action == TRADE_ACTION_DEAL || p.trade_action == TRADE_ACTION_PENDING { validate_volume(p.volume)?; }
        let digits = self.resolve_digits(p.symbol, p.digits);
        self.trade_request(&TradeRequest {
            trade_action: p.trade_action,
            symbol: p.symbol.to_string(),
            volume: Self::volume_to_lots(p.volume, 8).max(0) as u64,
            digits,
            trade_type: p.trade_type,
            type_filling: p.filling,
            type_time: p.time_type,
            price_order: p.price,
            price_trigger: p.trigger,
            price_sl: p.sl,
            price_tp: p.tp,
            deviation: p.deviation,
            comment: p.comment.to_string(),
            time_expiration: p.expiration,
            type_reason: p.magic,
            ..Default::default()
        })
        .await
    }

    async fn market(&self, symbol: &str, volume: f64, trade_type: i64, o: &MarketOrderOptions) -> Result<TradeResult> {
        self.place_order(Place {
            trade_action: TRADE_ACTION_DEAL, symbol, volume, trade_type, digits: o.digits, price: 0.0, trigger: 0.0, sl: o.sl, tp: o.tp,
            deviation: o.deviation, comment: &o.comment, filling: o.filling, time_type: ORDER_TIME_GTC, expiration: 0, magic: o.magic,
        })
        .await
    }

    async fn pending(&self, symbol: &str, volume: f64, trade_type: i64, price: f64, trigger: f64, o: &PendingOrderOptions) -> Result<TradeResult> {
        self.place_order(Place {
            trade_action: TRADE_ACTION_PENDING, symbol, volume, trade_type, digits: o.digits, price, trigger, sl: o.sl, tp: o.tp,
            deviation: 0, comment: &o.comment, filling: o.filling, time_type: o.time_type, expiration: o.expiration, magic: o.magic,
        })
        .await
    }

    pub async fn buy_market(&self, symbol: &str, volume: f64, o: &MarketOrderOptions) -> Result<TradeResult> { self.market(symbol, volume, ORDER_TYPE_BUY, o).await }
    pub async fn sell_market(&self, symbol: &str, volume: f64, o: &MarketOrderOptions) -> Result<TradeResult> { self.market(symbol, volume, ORDER_TYPE_SELL, o).await }
    pub async fn buy_limit(&self, symbol: &str, volume: f64, price: f64, o: &PendingOrderOptions) -> Result<TradeResult> { self.pending(symbol, volume, ORDER_TYPE_BUY_LIMIT, price, 0.0, o).await }
    pub async fn sell_limit(&self, symbol: &str, volume: f64, price: f64, o: &PendingOrderOptions) -> Result<TradeResult> { self.pending(symbol, volume, ORDER_TYPE_SELL_LIMIT, price, 0.0, o).await }
    pub async fn buy_stop(&self, symbol: &str, volume: f64, price: f64, o: &PendingOrderOptions) -> Result<TradeResult> { self.pending(symbol, volume, ORDER_TYPE_BUY_STOP, price, 0.0, o).await }
    pub async fn sell_stop(&self, symbol: &str, volume: f64, price: f64, o: &PendingOrderOptions) -> Result<TradeResult> { self.pending(symbol, volume, ORDER_TYPE_SELL_STOP, price, 0.0, o).await }

    /// Buy stop-limit: `price` is the stop trigger, `stop_limit_price` the limit placed after trigger.
    pub async fn buy_stop_limit(&self, symbol: &str, volume: f64, price: f64, stop_limit_price: f64, o: &PendingOrderOptions) -> Result<TradeResult> {
        self.pending(symbol, volume, ORDER_TYPE_BUY_STOP_LIMIT, stop_limit_price, price, o).await
    }

    pub async fn sell_stop_limit(&self, symbol: &str, volume: f64, price: f64, stop_limit_price: f64, o: &PendingOrderOptions) -> Result<TradeResult> {
        self.pending(symbol, volume, ORDER_TYPE_SELL_STOP_LIMIT, stop_limit_price, price, o).await
    }

    /// Close a position with an opposite market order (direction auto-detected unless given).
    pub async fn close_position(&self, symbol: &str, position_id: u64, volume: f64, o: &CloseOptions) -> Result<TradeResult> {
        let digits = self.resolve_digits(symbol, o.digits);
        let order_type = match o.order_type { Some(t) => t, None => self.detect_close_direction(position_id).await? };
        self.trade_request(&TradeRequest {
            trade_action: TRADE_ACTION_DEAL,
            symbol: symbol.to_string(),
            volume: Self::volume_to_lots(volume, 8).max(0) as u64,
            digits,
            trade_type: order_type,
            type_filling: o.filling,
            deviation: o.deviation,
            comment: o.comment.clone(),
            position_id,
            type_reason: o.magic,
            ..Default::default()
        })
        .await
    }

    async fn detect_close_direction(&self, position_id: u64) -> Result<i64> {
        match self.get_positions().await {
            Ok(positions) => {
                for p in &positions {
                    if p.get_u64("position_id") == position_id {
                        return Ok(if p.get_i64("trade_action") == POSITION_TYPE_SELL { ORDER_TYPE_BUY } else { ORDER_TYPE_SELL });
                    }
                }
                warn!("position {position_id} not found in open positions; defaulting to SELL (may open an unwanted short position)");
            }
            Err(e) if is_soft_error(&e) => debug!("failed to detect position direction: {e}"),
            Err(e) => return Err(e),
        }
        Ok(ORDER_TYPE_SELL)
    }

    /// Close `position_id` against the opposite position `position_by` (same symbol).
    pub async fn close_position_by(&self, symbol: &str, position_id: u64, position_by: u64, filling: i64, digits: Option<u32>, magic: i64) -> Result<TradeResult> {
        let digits = self.resolve_digits(symbol, digits);
        self.trade_request(&TradeRequest {
            trade_action: TRADE_ACTION_CLOSE_BY, symbol: symbol.to_string(), digits, type_filling: filling, position_id, position_by, type_reason: magic,
            ..Default::default()
        })
        .await
    }

    pub async fn modify_position_sltp(&self, symbol: &str, position_id: u64, sl: f64, tp: f64) -> Result<TradeResult> {
        self.trade_request(&TradeRequest { trade_action: TRADE_ACTION_SLTP, symbol: symbol.to_string(), position_id, price_sl: sl, price_tp: tp, ..Default::default() }).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn modify_pending_order(&self, symbol: &str, order: u64, price: f64, sl: f64, tp: f64, time_type: i64, expiration: i64) -> Result<TradeResult> {
        self.trade_request(&TradeRequest {
            trade_action: TRADE_ACTION_MODIFY, symbol: symbol.to_string(), order, price_order: price, price_sl: sl, price_tp: tp, type_time: time_type,
            time_expiration: expiration, ..Default::default()
        })
        .await
    }

    pub async fn cancel_pending_order(&self, order: u64) -> Result<TradeResult> {
        self.trade_request(&TradeRequest { trade_action: TRADE_ACTION_REMOVE, order, ..Default::default() }).await
    }
}
