//! # rustmt5
//!
//! Async Rust client for the MetaTrader 5 Web Terminal binary WebSocket protocol — a port of
//! the Python library [`pymt5`](https://github.com/cloudQuant/pymt5).
//!
//! ```no_run
//! use rustmt5::{ClientConfig, Mt5Client};
//!
//! # async fn demo() -> rustmt5::Result<()> {
//! let client = Mt5Client::new(ClientConfig::new("wss://web.metatrader.app/terminal"));
//! client.connect().await?;
//! client.login(12345678, "password").await?;
//! let account = client.get_account().await?;
//! println!("balance = {}", account.get_f64("balance"));
//! client.close().await;
//! # Ok(()) }
//! ```

mod account;
mod client;
pub mod constants;
mod constants_gen;
pub mod crypto;
mod currency;
pub mod error;
pub mod helpers;
mod market_data;
pub mod metrics;
mod order_helpers;
pub mod order_manager;
pub mod parsers;
pub mod pool;
pub mod protocol;
mod push_handlers;
pub mod rate_limiter;
pub mod schemas;
pub mod subscription;
pub mod trading;
pub mod transport;
pub mod types;
pub mod validation;

pub use client::{observed_build_release_date, ClientConfig, LoginParams, Mt5Client, MT5_TERMINAL_VERSION};
pub use error::{Error, Result};
pub use metrics::MetricsCollector;
pub use order_helpers::{CloseOptions, MarketOrderOptions, PendingOrderOptions};
pub use order_manager::{OrderManager, OrderState, PositionSummary, TrackedOrder};
pub use parsers::TimeArg;
pub use pool::{Mt5ConnectionPool, PoolAccount};
pub use protocol::{Record, RecordList, Value};
pub use push_handlers::TradeUpdate;
pub use subscription::SubscriptionHandle;
pub use trading::{HistoryFilter, PositionsAndOrders, TradeHistory, TradeRequest};
pub use transport::{CommandResult, Transport, TransportState};
pub use types::*;
