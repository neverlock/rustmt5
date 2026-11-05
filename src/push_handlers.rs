//! Push notification handlers and tick/book caches (port of `pymt5._push_handlers`).
//!
//! Handlers are plain closures executed on the transport receive task: keep them short and
//! hand heavy work to a channel / spawned task.

use tracing::debug;

use crate::client::{Mt5Client, State};
use crate::constants::*;
use crate::protocol::{get_series_size, Record, RecordList, SeriesCodec, Value};
use crate::parsers::{parse_account_response, parse_book_entries, parse_counted_records, parse_tick_batch};
use crate::schemas::*;
use crate::transport::{CallbackError, CommandResult, ListenerId};
use crate::types::{AccountEvent, BookEvent, TickEvent, TradeResultEvent};

/// Positions and orders carried by a cmd=4 push.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TradeUpdate {
    pub positions: RecordList,
    pub orders: RecordList,
}

fn read_u32(body: &[u8], off: usize) -> Option<u32> { body.get(off..off + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())) }

/// Split a cmd=4 body into `(positions, orders)`.
pub(crate) fn split_positions_orders(body: &[u8]) -> (RecordList, RecordList) {
    let positions = parse_counted_records(body, POSITION_SCHEMA, POSITION_FIELD_NAMES);
    let pos_size = get_series_size(POSITION_SCHEMA).unwrap();
    let pos_count = read_u32(body, 0).unwrap_or(0) as usize;
    let order_offset = 4usize.saturating_add(pos_count.saturating_mul(pos_size));
    let orders = parse_counted_records(body.get(order_offset..).unwrap_or(&[]), ORDER_SCHEMA, ORDER_FIELD_NAMES);
    (positions, orders)
}

/// Parse a cmd=19 body into `{...action fields, "result": {...}}`.
pub(crate) fn parse_trade_result_push(body: &[u8]) -> Record {
    let action_size = get_series_size(TRADE_RESULT_PUSH_SCHEMA).unwrap();
    let resp_size = get_series_size(TRADE_RESULT_RESPONSE_SCHEMA).unwrap();
    let mut data = Record::new();
    if body.len() >= action_size {
        if let Ok(r) = SeriesCodec::parse_record(body, TRADE_RESULT_PUSH_SCHEMA, TRADE_RESULT_PUSH_FIELD_NAMES, 0) {
            data.extend(r.0);
        }
    }
    if body.len() >= action_size + resp_size {
        if let Ok(r) = SeriesCodec::parse_record(body, TRADE_RESULT_RESPONSE_SCHEMA, TRADE_RESULT_RESPONSE_FIELD_NAMES, action_size) {
            data.set("result", r);
        }
    }
    data
}

fn parse_trade_transaction(body: &[u8]) -> Option<Record> {
    let update_type = read_u32(body, 0)?;
    let mut data = Record::new().with("update_type", update_type);
    let mut offset = 4usize;
    if update_type == 2 {
        let bal = get_series_size(TRADE_UPDATE_BALANCE_SCHEMA).unwrap();
        if body.len() >= offset + bal {
            if let Ok(r) = SeriesCodec::parse_record(body, TRADE_UPDATE_BALANCE_SCHEMA, TRADE_UPDATE_BALANCE_FIELD_NAMES, offset) {
                data.set("balance_info", r);
            }
            offset += bal;
        }
        let rest = body.get(offset..).unwrap_or(&[]);
        data.set("deals", parse_counted_records(rest, DEAL_SCHEMA, DEAL_FIELD_NAMES));
        let deal_size = get_series_size(DEAL_SCHEMA).unwrap();
        if let Some(deal_count) = read_u32(body, offset) {
            offset = offset.saturating_add(4).saturating_add((deal_count as usize).saturating_mul(deal_size));
        }
        data.set("positions", parse_counted_records(body.get(offset..).unwrap_or(&[]), POSITION_SCHEMA, POSITION_FIELD_NAMES));
    } else {
        let txn = get_series_size(TRADE_TRANSACTION_SCHEMA).unwrap();
        if body.len() >= offset + txn {
            if let Ok(r) = SeriesCodec::parse_record(body, TRADE_TRANSACTION_SCHEMA, TRADE_TRANSACTION_FIELD_NAMES, offset) {
                data.extend(r.0);
            }
            offset += txn;
        }
        let order = get_series_size(ORDER_SCHEMA).unwrap();
        if body.len() >= offset + order {
            if let Ok(r) = SeriesCodec::parse_record(body, ORDER_SCHEMA, ORDER_FIELD_NAMES, offset) {
                data.set("order", r);
            }
        }
    }
    Some(data)
}

fn tick_event(tick: &Record) -> TickEvent {
    let ts_ms = tick.get_f64("tick_time_ms");
    TickEvent {
        symbol_id: tick.get_u64("symbol_id") as u32,
        symbol: tick.get_str("symbol").to_string(),
        bid: tick.get_f64("bid"),
        ask: tick.get_f64("ask"),
        last: tick.get_f64("last"),
        volume: tick.get_f64("tick_volume"),
        timestamp: if ts_ms != 0.0 { ts_ms } else { tick.get_f64("tick_time") },
        raw: tick.clone(),
    }
}

impl Mt5Client {
    /// Snapshot-parse ticks using the current symbol + tick caches.
    fn parse_ticks(state: &std::sync::Mutex<State>, body: &[u8]) -> RecordList {
        let st = state.lock().unwrap_or_else(|e| e.into_inner());
        parse_tick_batch(body, &st.symbols_by_id, Some(&st.tick_cache_by_id))
    }

    /// Ticks (`cmd=8`). The callback receives all ticks of one push batch.
    pub fn on_tick(&self, cb: impl Fn(&[Record]) + Send + Sync + 'static) -> ListenerId {
        let state = self.inner.state.clone();
        self.transport().on(CMD_TICK_PUSH, move |r| cb(&Self::parse_ticks(&state, &r.body)))
    }

    /// Positions part of the cmd=4 push.
    pub fn on_position_update(&self, cb: impl Fn(&[Record]) + Send + Sync + 'static) -> ListenerId {
        self.transport().on(CMD_GET_POSITIONS_ORDERS, move |r| cb(&split_positions_orders(&r.body).0))
    }

    /// Orders part of the cmd=4 push.
    pub fn on_order_update(&self, cb: impl Fn(&[Record]) + Send + Sync + 'static) -> ListenerId {
        self.transport().on(CMD_GET_POSITIONS_ORDERS, move |r| cb(&split_positions_orders(&r.body).1))
    }

    /// Both positions and orders of the cmd=4 push (more efficient than two handlers).
    pub fn on_trade_update(&self, cb: impl Fn(&TradeUpdate) + Send + Sync + 'static) -> ListenerId {
        self.transport().on(CMD_GET_POSITIONS_ORDERS, move |r| {
            let (positions, orders) = split_positions_orders(&r.body);
            cb(&TradeUpdate { positions, orders });
        })
    }

    /// Raw symbol update push (`cmd=13`; schema varies by server).
    pub fn on_symbol_update(&self, cb: impl Fn(&CommandResult) + Send + Sync + 'static) -> ListenerId { self.transport().on(CMD_SYMBOL_UPDATE_PUSH, cb) }

    /// Raw login status push (`cmd=15`), e.g. forced logout.
    pub fn on_login_status(&self, cb: impl Fn(&CommandResult) + Send + Sync + 'static) -> ListenerId { self.transport().on(CMD_LOGIN_STATUS_PUSH, cb) }

    /// Account balance/margin push (`cmd=14`), same fields as [`Mt5Client::get_account`].
    pub fn on_account_update(&self, cb: impl Fn(&Record) + Send + Sync + 'static) -> ListenerId {
        self.transport().on(CMD_ACCOUNT_UPDATE_PUSH, move |r| cb(&parse_account_response(&r.body)))
    }

    /// Extended quote data incl. option greeks (`cmd=17`).
    pub fn on_symbol_details(&self, cb: impl Fn(&[Record]) + Send + Sync + 'static) -> ListenerId {
        let state = self.inner.state.clone();
        let size = get_series_size(SYMBOL_DETAILS_SCHEMA).unwrap();
        self.transport().on(CMD_SYMBOL_DETAILS_PUSH, move |r| {
            let mut out = Vec::new();
            for i in 0..r.body.len() / size {
                let Ok(mut d) = SeriesCodec::parse_record(&r.body, SYMBOL_DETAILS_SCHEMA, SYMBOL_DETAILS_FIELD_NAMES, i * size) else { break };
                let id = d.get_u64("symbol_id") as u32;
                if let Some(s) = state.lock().unwrap_or_else(|e| e.into_inner()).symbols_by_id.get(&id) {
                    d.set("symbol", s.name.as_str());
                }
                out.push(d);
            }
            cb(&out);
        })
    }

    /// Async trade execution results (`cmd=19`).
    pub fn on_trade_result(&self, cb: impl Fn(&Record) + Send + Sync + 'static) -> ListenerId {
        self.transport().on(CMD_TRADE_RESULT_PUSH, move |r| cb(&parse_trade_result_push(&r.body)))
    }

    /// Trade transactions / balance updates (`cmd=10`).
    ///
    /// `update_type == 2` carries `balance_info`, `deals`, `positions`; otherwise
    /// `flag_mask`, `transaction_id`, `transaction_type` and `order`.
    pub fn on_trade_transaction(&self, cb: impl Fn(&Record) + Send + Sync + 'static) -> ListenerId {
        self.transport().on(CMD_TRADE_UPDATE_PUSH, move |r| {
            if let Some(d) = parse_trade_transaction(&r.body) { cb(&d); }
        })
    }

    /// Order book / DOM push (`cmd=23`): records with `symbol_id`, `bids`, `asks`.
    pub fn on_book_update(&self, cb: impl Fn(&[Record]) + Send + Sync + 'static) -> ListenerId {
        let state = self.inner.state.clone();
        self.transport().on(CMD_BOOK_PUSH, move |r| {
            let entries = parse_book_entries(&r.body, &state.lock().unwrap_or_else(|e| e.into_inner()).symbols_by_id);
            cb(&entries);
        })
    }

    // ---- typed handlers ----

    pub fn on_tick_event(&self, cb: impl Fn(&TickEvent) + Send + Sync + 'static) -> ListenerId {
        let state = self.inner.state.clone();
        self.transport().on(CMD_TICK_PUSH, move |r| {
            for t in Self::parse_ticks(&state, &r.body) { cb(&tick_event(&t)); }
        })
    }

    pub fn on_book_event(&self, cb: impl Fn(&BookEvent) + Send + Sync + 'static) -> ListenerId {
        let state = self.inner.state.clone();
        self.transport().on(CMD_BOOK_PUSH, move |r| {
            let entries = parse_book_entries(&r.body, &state.lock().unwrap_or_else(|e| e.into_inner()).symbols_by_id);
            for e in entries {
                let mut levels: Vec<Record> = e.get_records("bids").into_iter().cloned().collect();
                levels.extend(e.get_records("asks").into_iter().cloned());
                cb(&BookEvent { symbol_id: e.get_u64("symbol_id") as u32, symbol: e.get_str("symbol").to_string(), entries: levels, raw: e });
            }
        })
    }

    pub fn on_trade_result_event(&self, cb: impl Fn(&TradeResultEvent) + Send + Sync + 'static) -> ListenerId {
        self.transport().on(CMD_TRADE_RESULT_PUSH, move |r| {
            let data = parse_trade_result_push(&r.body);
            let res = data.get_record("result").cloned().unwrap_or_default();
            cb(&TradeResultEvent {
                retcode: res.get_i64("retcode"),
                order: res.get_u64("order"),
                deal: res.get_u64("deal"),
                volume: res.get_f64("volume"),
                price: res.get_f64("price"),
                comment: res.get_str("comment").to_string(),
                raw: data,
            });
        })
    }

    pub fn on_account_event(&self, cb: impl Fn(&AccountEvent) + Send + Sync + 'static) -> ListenerId {
        self.transport().on(CMD_ACCOUNT_UPDATE_PUSH, move |r| {
            let d = parse_account_response(&r.body);
            if d.is_empty() { return; }
            cb(&AccountEvent { balance: d.get_f64("balance"), equity: d.get_f64("equity"), margin: d.get_f64("margin"), margin_free: d.get_f64("margin_free"), raw: d });
        })
    }

    /// Handler invoked when a push callback panics.
    pub fn on_callback_error(&self, cb: impl Fn(&CallbackError) + Send + Sync + 'static) {
        self.transport().add_callback_error_handler(std::sync::Arc::new(cb));
    }

    // ---- caches ----

    pub(crate) fn install_cache_listeners(&self) {
        let state = self.inner.state.clone();
        let limit = self.inner.config.tick_history_limit;
        let max_symbols = self.inner.config.max_tick_symbols;
        self.transport().on(CMD_TICK_PUSH, move |r| {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            let st = &mut *guard;
            let ticks = parse_tick_batch(&r.body, &st.symbols_by_id, Some(&st.tick_cache_by_id));
            for tick in ticks {
                let id = tick.get_u64("symbol_id") as u32;
                st.tick_cache_by_id.insert(id, tick.clone());
                let hist = st.tick_history_by_id.entry(id).or_default();
                if limit > 0 && hist.len() >= limit { hist.pop_front(); }
                hist.push_back(tick.clone());
                st.tick_history_access_order.retain(|i| *i != id);
                st.tick_history_access_order.push(id);
                if max_symbols > 0 {
                    while st.tick_history_by_id.len() > max_symbols && !st.tick_history_access_order.is_empty() {
                        let evict = st.tick_history_access_order.remove(0);
                        st.tick_history_by_id.remove(&evict);
                    }
                }
                if let Some(name) = tick.get("symbol").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    st.tick_cache_by_name.insert(name.to_string(), tick.clone());
                }
            }
        });
        let state = self.inner.state.clone();
        self.transport().on(CMD_BOOK_PUSH, move |r| {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            let entries = parse_book_entries(&r.body, &st.symbols_by_id);
            for entry in entries {
                let id = entry.get_u64("symbol_id") as u32;
                if let Some(name) = entry.get("symbol").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    st.book_cache_by_name.insert(name.to_string(), entry.clone());
                }
                st.book_cache_by_id.insert(id, entry);
            }
        });
        debug!("cache listeners installed");
    }

    /// Clear tick history for one symbol id, or everything when `None`.
    pub fn clear_tick_history(&self, symbol_id: Option<u32>) {
        let mut st = self.st();
        match symbol_id {
            Some(id) => {
                st.tick_history_by_id.remove(&id);
                st.tick_history_access_order.retain(|i| *i != id);
            }
            None => {
                st.tick_history_by_id.clear();
                st.tick_history_access_order.clear();
            }
        }
    }
}

