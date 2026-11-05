# Architecture

## 1. Layers

```
+--------------------------------------------------------------------+
| Mt5Client  (src/client.rs + impl blocks in the other files)        |
|   account | market_data | currency | trading | order_helpers |     |
|   push_handlers          <- pymt5's mixins became impl blocks      |
+--------------------------------------------------------------------+
| Transport  (src/transport.rs)                                      |
|   WebSocket | state machine | FIFO pending | listeners | rate limit|
+--------------------------------------------------------------------+
| AesCipher  (src/crypto.rs)   AES-CBC, IV = 0, PKCS7                |
+--------------------------------------------------------------------+
| protocol   (src/protocol.rs) framing + SeriesCodec + Value/Record  |
+--------------------------------------------------------------------+
        shared: constants | schemas (generated) | parsers | helpers
```

Dependencies only point downwards: `Transport` does not know `Mt5Client` (disconnects are reported through a callback),
so the transport can be tested alone.

## 2. File map

| File | Responsibility | pymt5 origin |
|---|---|---|
| `lib.rs` | public re-exports, crate docs | `__init__.py` |
| `constants.rs`, `constants_gen.rs` | command ids, trade constants, retcode text, calc-mode sets (`_gen` is generated) | `constants.py` |
| `schemas.rs` | schemas + field names (**generated**) | `schemas.py`, part of `types.py` |
| `protocol.rs` | `PropType/Field`, `Value/Record`, `SeriesCodec`, framing | `protocol.py` |
| `helpers.rs` | hex, UTF-16LE, ASCII padding, client id | `helpers.py` |
| `crypto.rs` | `AesCipher`, initial key | `crypto.py` |
| `transport.rs` | `Transport`, `TransportState`, `CommandResult` | `transport.py` |
| `rate_limiter.rs`, `metrics.rs`, `error.rs`, `validation.rs` | token bucket, metrics trait, `Error`, input checks | `_rate_limiter.py`, `_metrics.py`, `exceptions.py`, `_validation.py` |
| `types.rs` | `TradeResult`, `AccountInfo`, `SymbolInfo`, requests, events | `types.py`, `events.py` |
| `parsers.rs` | response parsers, tick normalisation, glob, order validation, push layouts | `_parsers.py` |
| `client.rs` | `Mt5Client` core: config, login, heartbeat, reconnect, health, live state | `client.py` |
| `account.rs` | account info, passwords, account opening, OTP | `_account.py` |
| `market_data.rs` | symbols, ticks, book, bars, `copy_*` | `_market_data.py` |
| `currency.rs` | currency conversion, profit/margin formulas, `account_margin` | `_currency.py` |
| `trading.rs` | positions/orders, history, `trade_request`, `order_send`, `order_check` | `_trading.py` |
| `order_helpers.rs` | `buy_market` ... `cancel_pending_order` (execution-mode aware) | `_order_helpers.py` |
| `push_handlers.rs` | `on_*` handlers, tick/book caches, live trade state | `_push_handlers.py` |
| `subscription.rs`, `order_manager.rs`, `pool.rs` | handles, order tracker, multi-account pool | `_subscription.py`, `_order_manager.py`, `_pool.py` |

## 3. Data flow of one request

```
caller -> Mt5Client::get_account()
            | build payload (SeriesCodec::serialize)
            v
        Transport::send_command(cmd, payload)
            | 1 rate_limiter.acquire()
            | 2 lock the sink (serialises sends)
            | 3 push (id, oneshot::Sender) onto pending[cmd]            <- FIFO per command
            | 4 inner = random(2) + cmd(u16) + payload -> AES -> pack_outer -> ws.send
            | 5 await the oneshot with a timeout
            v
      recv task (loops on the WebSocket)
            | unpack_outer -> decrypt -> parse_response_frame
            | dispatch: wake the first live waiter of pending[cmd] AND call every listener of cmd
            v
        caller <- CommandResult { command, code, body }
            | parse body (parsers.rs) -> Record / struct
```

Responses carry **no request id**: they are matched by command number only, hence the per-command FIFO. Pushes that share a
command number with a response (cmd 4, 14) reach both the waiter and the listeners. Trading adds its own correlation
(`action_id`, see [trading.md](trading.md)).

## 4. Concurrency model

* **Tokio** multi-thread runtime. `Mt5Client` is `Clone` (an `Arc<Inner>`); all clones share one connection.
* Long-lived tasks: the **recv task** (in `Transport`), the **heartbeat** task and, when enabled, the **reconnect** task.
* **Listeners are synchronous closures that run on the recv task**: never block or `.await` in them. Hand work to a channel or
  `tokio::spawn`. A panicking listener is caught (`catch_unwind`) and reported through `on_callback_error`.

## 5. Locking rules (important when editing)

| State | Type | Rule |
|---|---|---|
| `Inner.state` (all client caches and live state) | `std::sync::Mutex<State>` | keep it short, **never across `.await`**; use `self.st()`; release before calling user callbacks |
| `Transport.sink` | `tokio::sync::Mutex` | held only while sending a frame (it doubles as the send lock) |
| `Transport.pending` / `listeners` | `std::sync::Mutex` | held only to push/pop; listeners are cloned out and called **outside** the lock |
| `Transport.cipher` | `std::sync::RwLock` | replaced only during connect (after bootstrap) |

Push handlers that need client state capture the `Arc<Mutex<State>>` (not the client) to avoid the cycle
`Mt5Client -> Transport -> listener -> Mt5Client`. The disconnect callback uses a `Weak<Inner>` for the same reason.

## 6. Structural differences from pymt5

| pymt5 | rustmt5 | Reason |
|---|---|---|
| mixin classes | several `impl Mt5Client` blocks | no inheritance; files stay small |
| `dict` everywhere | `Record` plus structs for dataclass-like results | keeps pymt5 semantics, typed top-level results |
| exceptions (multiple inheritance) | `enum Error` + `Result` | see [types-and-errors.md](types-and-errors.md) |
| `async with client` | explicit `connect()`/`close()` | no async `Drop` |
| sync or async callbacks | sync closures | simpler; see section 4 |
| reconnect builds a new transport | reuses the transport | listeners survive ([client.md](client.md)) |
