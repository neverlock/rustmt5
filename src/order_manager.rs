//! Order lifecycle tracker (port of `pymt5._order_manager`).

use std::collections::HashMap;

use tracing::{debug, error};

use crate::protocol::Record;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderState {
    Pending,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackedOrder {
    pub order_id: u64,
    pub symbol: String,
    pub order_type: i64,
    pub volume: f64,
    pub price: f64,
    pub state: OrderState,
    pub filled_volume: f64,
    pub fill_price: f64,
    pub sl: f64,
    pub tp: f64,
    pub comment: String,
    pub raw: Record,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PositionSummary {
    pub symbol: String,
    /// positive = long, negative = short
    pub net_volume: f64,
    pub avg_price: f64,
    pub unrealized_pnl: f64,
    pub order_count: u32,
}

/// Optional extras for [`OrderManager::track_order`].
#[derive(Debug, Clone, Default)]
pub struct TrackOptions {
    pub sl: f64,
    pub tp: f64,
    pub comment: String,
    pub raw: Record,
}

type StateCb = Box<dyn Fn(&TrackedOrder, OrderState) + Send + Sync>;

/// In-memory registry of orders and their state transitions.
#[derive(Default)]
pub struct OrderManager {
    orders: HashMap<u64, TrackedOrder>,
    positions: HashMap<String, PositionSummary>,
    callbacks: Vec<StateCb>,
}

impl OrderManager {
    pub fn new() -> Self { Self::default() }

    /// Begin tracking an order (replaces an existing one with the same id).
    pub fn track_order(&mut self, order_id: u64, symbol: &str, order_type: i64, volume: f64, price: f64, opts: TrackOptions) -> &TrackedOrder {
        debug!("tracking order {order_id} {symbol} vol={volume:.4} price={price:.5}");
        self.orders.insert(order_id, TrackedOrder {
            order_id, symbol: symbol.into(), order_type, volume, price, state: OrderState::Pending, filled_volume: 0.0, fill_price: 0.0,
            sl: opts.sl, tp: opts.tp, comment: opts.comment, raw: opts.raw,
        });
        &self.orders[&order_id]
    }

    /// Update from a trade result record (`order`, `retcode`, optional `volume`, `price`, `comment`).
    pub fn update_from_trade_result(&mut self, tr: &Record) -> Option<&TrackedOrder> {
        let id = tr.get_u64("order");
        if id == 0 { return None; }
        let order = self.orders.get_mut(&id)?;
        let old = order.state;
        match tr.get_i64("retcode") {
            10009 => { order.state = OrderState::Filled; order.filled_volume = order.volume; }
            10010 => {
                order.state = OrderState::PartiallyFilled;
                let filled = tr.get_f64("volume");
                if filled != 0.0 { order.filled_volume = filled; }
            }
            10006 => order.state = OrderState::Rejected,
            10007 => order.state = OrderState::Canceled,
            10008 => order.state = OrderState::Pending,
            _ => {}
        }
        if tr.get_f64("price") != 0.0 { order.fill_price = tr.get_f64("price"); }
        if tr.contains_key("comment") { order.comment = tr.get_str("comment").to_string(); }
        order.raw.extend(tr.0.clone());
        let snapshot = order.clone();
        if snapshot.state != old { self.fire(&snapshot, old); }
        self.orders.get(&id)
    }

    /// Update from a push record (`order`/`trade_order`, optional `state`, `volume_current`).
    pub fn update_from_push(&mut self, push: &Record) {
        let id = if push.get_u64("order") != 0 { push.get_u64("order") } else { push.get_u64("trade_order") };
        if id == 0 { return; }
        let Some(order) = self.orders.get_mut(&id) else { return };
        let old = order.state;
        if let Some(s) = push.get("state").and_then(|v| v.as_i64()) {
            order.state = match s {
                0 | 1 => OrderState::Pending,
                2 => OrderState::Canceled,
                3 => OrderState::PartiallyFilled,
                4 => OrderState::Filled,
                5 => OrderState::Rejected,
                6 => OrderState::Expired,
                _ => order.state,
            };
        }
        if push.contains_key("volume_current") { order.filled_volume = order.volume - push.get_f64("volume_current"); }
        order.raw.extend(push.0.clone());
        let snapshot = order.clone();
        if snapshot.state != old { self.fire(&snapshot, old); }
    }

    pub fn get_order(&self, id: u64) -> Option<&TrackedOrder> { self.orders.get(&id) }
    pub fn get_orders(&self, state: Option<OrderState>) -> Vec<&TrackedOrder> {
        self.orders.values().filter(|o| state.is_none_or(|s| o.state == s)).collect()
    }
    pub fn get_position(&self, symbol: &str) -> Option<&PositionSummary> { self.positions.get(symbol) }
    pub fn get_positions(&self) -> Vec<&PositionSummary> { self.positions.values().collect() }

    /// Callback receives `(order_with_new_state, old_state)`.
    pub fn on_state_change(&mut self, cb: impl Fn(&TrackedOrder, OrderState) + Send + Sync + 'static) { self.callbacks.push(Box::new(cb)); }

    fn fire(&self, order: &TrackedOrder, old: OrderState) {
        for cb in &self.callbacks {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(order, old))).is_err() {
                error!("state_change callback panicked");
            }
        }
    }

    pub fn clear(&mut self) {
        self.orders.clear();
        self.positions.clear();
        debug!("order manager cleared");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn lifecycle_and_callbacks() {
        let mut m = OrderManager::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        m.on_state_change(move |o, old| s2.lock().unwrap().push((o.state, old)));
        m.track_order(7, "EURUSD", 0, 1.0, 1.1, TrackOptions::default());
        assert!(m.update_from_trade_result(&Record::new().with("order", 99u64)).is_none());
        assert!(m.update_from_trade_result(&Record::new()).is_none());
        let o = m.update_from_trade_result(&Record::new().with("order", 7u64).with("retcode", 10009i64).with("price", 1.2f64)).unwrap();
        assert_eq!((o.state, o.filled_volume, o.fill_price), (OrderState::Filled, 1.0, 1.2));
        m.update_from_push(&Record::new().with("order", 7u64).with("state", 2i64).with("volume_current", 0.25f64));
        assert_eq!(m.get_order(7).unwrap().state, OrderState::Canceled);
        assert_eq!(m.get_order(7).unwrap().filled_volume, 0.75);
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(m.get_orders(Some(OrderState::Canceled)).len(), 1);
        assert!(m.get_orders(Some(OrderState::Pending)).is_empty());
        m.clear();
        assert!(m.get_orders(None).is_empty());
    }

    #[test]
    fn panicking_callback_is_contained() {
        let mut m = OrderManager::new();
        m.on_state_change(|_, _| panic!("boom"));
        m.track_order(1, "X", 0, 1.0, 1.0, TrackOptions::default());
        m.update_from_trade_result(&Record::new().with("order", 1u64).with("retcode", 10006i64));
        assert_eq!(m.get_order(1).unwrap().state, OrderState::Rejected);
    }
}
