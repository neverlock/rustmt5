//! Push notification handlers and tick/book caches (port of `pymt5._push_handlers`).
//!
//! Handlers are plain closures executed on the transport receive task: keep them short and
//! hand heavy work to a channel / spawned task.

use tracing::debug;

use crate::client::{Mt5Client, State};
use crate::constants::*;
use crate::protocol::{get_series_size, Record, RecordList, SeriesCodec, Value};
use crate::parsers::{parse_account_response, parse_book_entries, parse_counted_records, parse_tick_batch, parse_trade_result_push, parse_trade_transaction};
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

/// Apply a parsed cmd=10 push to the live position/order maps.
pub(crate) fn apply_trade_update(st: &mut State, update: &Record) {
    match update.get_i64("update_type") {
        2 => {
            for pos in update.get_records("positions") {
                let id = pos.get_u64("position_id");
                if id == 0 { continue; }
                if pos.get_u64("trade_volume") == 0 { st.live_positions.shift_remove(&id); } else { st.live_positions.insert(id, pos.clone()); }
            }
        }
        // open-orders update: transaction_type 0 add, 1 update, 2 delete. A cancelled order is *not* deleted this way: it
        // gets an update with a non-live state (9 = cancel requested) and then a history push (below).
        0 => {
            if let Some(order) = update.get_record("order") {
                let id = order.get_u64("trade_order");
                if id == 0 { return; }
                // ORDER_STATE: 0 started, 1 placed, 3 partial, 7/8 add/modify requested are live
                let live = matches!(order.get_i64("order_state"), 0 | 1 | 3 | 7 | 8);
                if update.get_i64("transaction_type") == 2 || !live { st.live_orders.shift_remove(&id); remember_finished_order(st, id); }
                // order ids are never reused: a late "live" update of an already finished order must not resurrect it
                else if !st.finished_orders.contains(&id) { st.live_orders.insert(id, order.clone()); }
            }
        }
        // history order added: the order is final (filled / cancelled / rejected / expired)
        1 => {
            if let Some(order) = update.get_record("order") {
                let id = order.get_u64("trade_order");
                st.live_orders.shift_remove(&id);
                remember_finished_order(st, id);
            }
        }
        _ => {}
    }
}

/// A request that completed with a deal (retcode 10009 "done") has a final order. The server does not always follow
/// with the history-order push that removes it from the live set; without this a filled market order lingers as a
/// pending order. Modify/cancel results also say 10009 but carry no deal, so they are left alone.
pub(crate) fn apply_trade_result(st: &mut State, data: &Record) {
    let Some(result) = data.get_record("result") else { return };
    let order = result.get_u64("order");
    if order != 0 && result.get_i64("retcode") == 10009 && result.get_u64("deal") != 0 {
        st.live_orders.shift_remove(&order);
        remember_finished_order(st, order);
    }
}

fn remember_finished_order(st: &mut State, id: u64) {
    if st.finished_orders.len() >= 4096 { st.finished_orders.clear(); }
    st.finished_orders.insert(id);
}

/// Close-by (`TRADE_ACTION_CLOSE_BY`): the server's cmd 10 push for it carries **no position records** (verified live,
/// build 6182), so the local state is updated from the request: both positions lose `min(volume)`; zero volume removes.
pub(crate) fn apply_close_by(st: &mut State, position_id: u64, position_by: u64) {
    let (Some(a), Some(b)) = (st.live_positions.get(&position_id), st.live_positions.get(&position_by)) else { return };
    let closed = a.get_u64("trade_volume").min(b.get_u64("trade_volume"));
    for id in [position_id, position_by] {
        let Some(p) = st.live_positions.get_mut(&id) else { continue };
        let left = p.get_u64("trade_volume").saturating_sub(closed);
        if left == 0 { st.live_positions.shift_remove(&id); } else { p.set("trade_volume", left); }
    }
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
        parse_tick_batch(body, &st.symbols_by_id, Some(&st.tick_cache_by_id), st.normalize_ticks)
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
            let ticks = parse_tick_batch(&r.body, &st.symbols_by_id, Some(&st.tick_cache_by_id), st.normalize_ticks);
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
        self.transport().on(CMD_TRADE_UPDATE_PUSH, move |r| {
            let Some(update) = parse_trade_transaction(&r.body) else { return };
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            if st.trade_state_ready { apply_trade_update(&mut st, &update); }
        });
        let state = self.inner.state.clone();
        self.transport().on(CMD_TRADE_RESULT_PUSH, move |r| {
            // forward to the trade_request() waiting for this action_id (if any); removal is done by the waiter
            let data = parse_trade_result_push(&r.body);
            let id = data.get_u64("action_id") as u32;
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            apply_trade_result(&mut st, &data);
            if let Some(tx) = st.trade_waiters.get(&id) { let _ = tx.send(data); }
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


#[cfg(test)]
mod tests {
    use super::*;

    fn pos(id: i64, vol: u64) -> Record { Record::new().with("position_id", id).with("trade_volume", vol) }
    fn order(id: i64, state: i64) -> Record { Record::new().with("trade_order", id).with("order_state", state) }
    fn upd(ty: i64, txn: i64, o: Option<Record>, p: Vec<Record>) -> Record {
        let mut r = Record::new().with("update_type", ty).with("transaction_type", txn);
        if let Some(o) = o { r.set("order", o); }
        r.set("positions", p);
        r
    }

    fn result(order: i64, retcode: i64, deal: i64) -> Record {
        Record::new().with("result", Record::new().with("order", order).with("retcode", retcode).with("deal", deal))
    }

    #[test]
    fn filled_market_order_is_not_left_pending() {
        let mut st = State::default();
        apply_trade_update(&mut st, &upd(0, 0, Some(order(5, 1)), vec![]));
        assert_eq!(st.live_orders.len(), 1);
        // placed, no deal yet: still live
        apply_trade_result(&mut st, &result(5, 10008, 0));
        assert_eq!(st.live_orders.len(), 1);
        // modify/cancel say 10009 without a deal: still live
        apply_trade_result(&mut st, &result(5, 10009, 0));
        assert_eq!(st.live_orders.len(), 1);
        // done with a deal: final
        apply_trade_result(&mut st, &result(5, 10009, 99));
        assert!(st.live_orders.is_empty());
        // a late "live" update must not resurrect it
        apply_trade_update(&mut st, &upd(0, 1, Some(order(5, 1)), vec![]));
        assert!(st.live_orders.is_empty());
    }

    #[test]
    fn live_state_follows_pushes() {
        let mut st = State::default();
        // positions: opened, resized, closed (volume 0)
        apply_trade_update(&mut st, &upd(2, 0, None, vec![pos(7, 100)]));
        apply_trade_update(&mut st, &upd(2, 0, None, vec![pos(7, 50), pos(8, 10)]));
        assert_eq!(st.live_positions.len(), 2);
        assert_eq!(st.live_positions[&7].get_u64("trade_volume"), 50);
        apply_trade_update(&mut st, &upd(2, 0, None, vec![pos(7, 0)]));
        assert_eq!(st.live_positions.keys().copied().collect::<Vec<_>>(), vec![8]);
        // orders: placed -> updated -> cancel requested (state 9) disappears
        apply_trade_update(&mut st, &upd(0, 0, Some(order(1, 1)), vec![]));
        apply_trade_update(&mut st, &upd(0, 1, Some(order(1, 1)), vec![]));
        assert_eq!(st.live_orders.len(), 1);
        apply_trade_update(&mut st, &upd(0, 1, Some(order(1, 9)), vec![]));
        assert!(st.live_orders.is_empty());
        // explicit delete and history push both remove
        apply_trade_update(&mut st, &upd(0, 0, Some(order(2, 1)), vec![]));
        apply_trade_update(&mut st, &upd(0, 2, Some(order(2, 1)), vec![]));
        assert!(st.live_orders.is_empty());
        apply_trade_update(&mut st, &upd(0, 0, Some(order(3, 1)), vec![]));
        apply_trade_update(&mut st, &upd(1, 0, Some(order(3, 4)), vec![]));
        assert!(st.live_orders.is_empty());
        // late out-of-order "live" push of a finished order is ignored
        apply_trade_update(&mut st, &upd(0, 0, Some(order(3, 0)), vec![]));
        assert!(st.live_orders.is_empty());
    }

    #[test]
    fn close_by_updates_local_positions() {
        let mut st = State::default();
        apply_trade_update(&mut st, &upd(2, 0, None, vec![pos(1, 200), pos(2, 200), pos(3, 500), pos(4, 200)]));
        apply_close_by(&mut st, 1, 2); // equal volumes: both gone
        assert_eq!(st.live_positions.keys().copied().collect::<Vec<_>>(), vec![3, 4]);
        apply_close_by(&mut st, 3, 4); // different volumes: the larger one keeps the difference
        assert_eq!(st.live_positions.keys().copied().collect::<Vec<_>>(), vec![3]);
        assert_eq!(st.live_positions[&3].get_u64("trade_volume"), 300);
    }
}
