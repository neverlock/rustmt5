# rustmt5

Async **Rust** client for the **MetaTrader 5 Web Terminal** reverse-engineered WebSocket binary
protocol. It is a port of the Python library [cloudQuant/pymt5](https://github.com/cloudQuant/pymt5)
(MIT) and keeps its layering and behaviour:

```
Mt5Client      (client, account, market_data, currency, trading, order_helpers, push_handlers)
   ↓
Transport      (WebSocket lifecycle, state machine, FIFO command queue, listeners, rate limit)
   ↓
AesCipher      (AES-CBC, zero IV, PKCS7; key from the bootstrap handshake)
   ↓
SeriesCodec    (schema driven binary serialisation; schemas generated from pymt5/schemas.py)
```

## Quick start

```rust
use rustmt5::{ClientConfig, Mt5Client};

#[tokio::main]
async fn main() -> rustmt5::Result<()> {
    let client = Mt5Client::new(ClientConfig::new("wss://web.metatrader.app/terminal"));
    client.connect().await?;
    client.login(12345678, "password").await?;

    let acct = client.get_account().await?;
    println!("balance {} {}", acct.get_f64("balance"), acct.get_str("currency"));

    client.load_symbols(true).await?;
    client.on_tick_event(|t| println!("{} {} / {}", t.symbol, t.bid, t.ask));
    let ids = client.subscribe_symbols(&["EURUSD"]).await?;
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    client.unsubscribe_ticks(&ids).await?;
    client.close().await;
    Ok(())
}
```

Data comes back as dynamic [`Record`]s (insertion-ordered `name → Value` maps, the Rust equivalent of
pymt5's dicts) with forgiving typed getters (`get_f64`, `get_i64`, `get_str`, `get_bool`,
`get_records`, ...). Dataclass-like results (`TradeResult`, `AccountInfo`, `SymbolInfo`, events, …) are
real structs.

## Feature parity with pymt5

| Area | API |
| --- | --- |
| Session | `connect`, `initialize`, `login`/`login_with`, `logout`, `ping`, `close`, heartbeat, auto-reconnect (exp. backoff + jitter), `health_check`, `send_raw_command` |
| Account | `get_account`, `account_info`, `terminal_info`, `version`, `get_account_summary`, `change_password`, `trader_params`, demo/real account opening, verification codes, OTP, `send_notification`, `get_corporate_links` |
| Market data | `load_symbols` (+TTL cache), `get_symbols`, `symbols_get`, `get_full_symbol_info`, `symbol_info`, `get_symbol_groups`, `get_spreads`, tick/book subscribe (+managed handles), `get_rates`, `copy_rates_*`, `copy_ticks_*`, `market_book_*` |
| Trading | `get_positions_and_orders`, `get_trade_history`, `positions_get`/`orders_get`/`history_*_get`, `trade_request`, `order_send`, `order_check`, `order_calc_profit`/`order_calc_margin`, `buy_market`…`sell_stop_limit`, `close_position`, `close_position_by`, `modify_position_sltp`, `modify_pending_order`, `cancel_pending_order` |
| Push | `on_tick`, `on_position_update`, `on_order_update`, `on_trade_update`, `on_account_update`, `on_trade_result`, `on_trade_transaction`, `on_book_update`, `on_symbol_update`, `on_symbol_details`, `on_login_status`, typed `on_*_event`, `on_callback_error` |
| Extras | `OrderManager`, `Mt5ConnectionPool`, `TokenBucketRateLimiter`, `MetricsCollector` |

## Differences from the Python library

* **Callbacks are synchronous closures** run on the receive task (keep them short; use a channel for
  heavy work). A panicking callback is caught and reported through `on_callback_error`.
* **Reconnect reuses the same `Transport`**, so registered listeners (including the tick/book caches)
  survive a reconnect. pymt5 creates a fresh transport and silently loses them.
* `login` checks the response code and returns `Error::Authentication` (pymt5 fails later with an
  opaque "buffer too short").
* `SubscriptionHandle` has no async `Drop`; call `unsubscribe().await` (a best-effort unsubscribe task
  is spawned if an active handle is dropped inside a Tokio runtime).
* Order helpers take option structs (`MarketOrderOptions`, `PendingOrderOptions`, `CloseOptions`) instead
  of keyword arguments; timestamps accept `i64`/`f64`/`SystemTime` through `TimeArg`.
* Not ported: `to_dataframe` (pandas) and the `PYMT5_DEBUG` / `PYMT5_LOG_LEVEL` switches - use a
  `tracing` subscriber instead.
* TLS uses rustls with the built-in web PKI roots. Extra roots (e.g. a corporate / intercepting proxy CA)
  can be added with `ClientConfig::ca_bundle` or the standard `SSL_CERT_FILE` variable. Verification is
  never disabled.
* Quirks of the original are preserved on purpose, e.g. an empty trade-settings count in the account
  response is not consumed, and `normalize_tick_price_value` heuristics are identical.

## Development

```bash
cargo test            # unit tests + end-to-end tests against an in-process mock MT5 server
cargo clippy --all-targets
```

`src/schemas.rs` and `src/constants_gen.rs` are generated from the Python sources (statically, via
`ast`, without executing them):

```bash
python3 tools/gen_schemas.py   /path/to/pymt5 src/schemas.rs
python3 tools/gen_constants.py /path/to/pymt5 src/constants_gen.rs
```

Live read-only connectivity check (never prints the password):

```bash
LOGIN=123456 PASSWORD=... SERVER=wss://host/terminal cargo run --example live_check
```

## Disclaimer

This is an unofficial client built on a reverse-engineered protocol. Use a demo account first; trading
involves risk and the authors accept no liability.

## License

MIT - see [LICENSE](LICENSE). Original work © cloudQuant.
