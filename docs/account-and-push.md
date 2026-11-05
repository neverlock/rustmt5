# Account and push notifications

## 1. Account response (cmd 3, also push cmd 14) - `parsers::parse_account_response`

The body is a sequence of sections:

| Section | Size | Notes |
|---|---|---|
| `ACCOUNT_WEB_MAIN` | 816 B | main fields ([schemas](reference/schemas.md)); shorter bodies return an **empty** `Record` |
| `u32 n` + `ACCOUNT_WEB_TRADE_SETTINGS` x n (628 B) | 4 + 628n | per-path price/volume/swap rules |
| `i32 trade_flags` | 4 | |
| `i32 symbols_count` | 4 | |
| `u64 leverage_flags` | 8 | |
| `i32 m` + (`LEVERAGE_RULE` 300 B + `tier_count` x `LEVERAGE_TIER` 32 B) x m | | `leverage_rules` (alias `rules`) |
| `u32 k` + (`COMMISSION` 304 B + `u32 t` + t x `COMMISSION_TIER` 80 B) x k | | `commissions` |

> **Quirk kept from pymt5:** when `n == 0` the 4-byte counter is *not* skipped, so `trade_flags`/`symbols_count` that follow are misread. Real servers send `n >= 1`;
> when writing a mock include at least one trade-settings record.

If parsing fails mid-way the whole result is an empty `Record` - callers should check `is_empty()`.

### Derived fields

| key | source |
|---|---|
| `currency`, `leverage`, `name`, `server` | `account_currency`, `margin_leverage`, `account_name`, `server_name` |
| `daylight_mode`, `server_offset_time` | `daylightmode`; `timezone_shift + daylight_mode*3600` |
| `is_real` / `is_demo` / `is_contest` / `is_preliminary` | `account_type` = 0 / 1 / 2 / 3 |
| `trade_allowed` (and `is_trade_disabled`) | `!(rights & 4)` |
| `is_investor`, `is_read_only`, `is_reset_pass` | `rights & 8`, `& 512`, `& 1024` |
| `is_hedged_margin` | `margin_mode == 2` |
| `risk_warning` | `permissions_flags & 16` |
| `profit` | `acc_profit` |
| `equity` | `balance + credit + profit` (**does not include floating profit if `acc_profit` is not pushed**; see `account_margin`) |
| `auth_password_min` | the value, or 8 when 0 |

**Not in the record:** `margin`, `margin_free`, `margin_level` (reading them gives 0). Use `account_margin()` ([trading.md](trading.md#5-margin-equity-and-profit-local)).

Verified live: live account `CXMDirect-Live`, USD, 1:1000, real; demo account `CXMDirect-Demo`, **USC (cent)**, 1:2000, `is_demo = true`.

### Methods built on cmd 3

| method | result |
|---|---|
| `get_account` / `account_info` | the `Record` above |
| `terminal_info` | MetaTrader5-style record (`build, company, name, server, connected, trade_allowed, tradeapi_disabled, timezone_shift, server_offset_time`; paths empty) |
| `version` | `Some((500, build, release_date))`; the date comes from a small table of observed builds (5687 -> "15 Mar 2026"), else `""` |
| `get_account_summary` | `AccountInfo` with locally computed margin figures and position/order counts |

## 2. Other account commands (not exercised against a real server)

| method | cmd | notes |
|---|---|---|
| `change_password(new, old, is_investor)` | 24 | returns an `i32` status - **changes the real password** |
| `trader_params` | 41 | `(String, String)` |
| `send_notification(msg)` | 42 | truncated to 128 characters |
| `get_corporate_links` | 44 | `link_type, url, label, flags, icon_data` |
| `verify_code(code)` | 27 | |
| `open_demo_account(&DemoAccountRequest, cid, initialize)` | 30 | `build_opening_base_payload` |
| `open_real_account(&RealAccountRequest, ...)` | 39 | base + extra fields + 512 B reserved + documents |
| `request_opening_verification` / `submit_opening_verification` | 27 / 40 | `VerificationStatus { email, phone }` |
| `enable_otp` / `disable_otp` | 43 | |

`build_opening_base_payload` writes 22 fields (full name `first second` truncated to 128, group, phone_password, country, city, state, zipcode, address, phone,
email, deposit f64, leverage u32, 0, 1, domain, utm_*, confirm codes, first/second name, agreements) = 1 664 B (unit-tested). `birth_date_ms` is a `Time` field.
`initialize = true` sends `init_session` (cmd 29) first with the same `cid`.

## 3. Push handlers

Every `on_*` returns a `ListenerId` (remove with `client.transport().off(cmd, Some(id))`). Callbacks are `Fn(..) + Send + Sync + 'static` closures running on the recv task.

| method | cmd | callback gets | parsing |
|---|---|---|---|
| `on_tick` | 8 | `&[Record]` (whole batch) | `parse_tick_batch` (+cache merge, +normalisation if enabled) |
| `on_tick_event` | 8 | `&TickEvent` (one tick) | `timestamp = tick_time_ms` (or `tick_time` when 0) |
| `on_position_update`, `on_order_update` | 4 | `&[Record]` | the matching part of cmd 4 (fires for **responses** too) |
| `on_trade_update` | 4 | `&TradeUpdate { positions, orders }` | whole cmd 4 body |
| `on_account_update` / `on_account_event` | 14 | `&Record` / `&AccountEvent` | `parse_account_response` (`margin*` are 0) |
| `on_trade_result` | 19 | `&Record` | `parse_trade_result_push`: request fields + `result: { retcode, deal, order, trade_order, volume, price, bid, ask, comment }` |
| `on_trade_result_event` | 19 | `&TradeResultEvent` | `retcode/order/deal/volume/price/comment` from `result` (one event **per push**, including intermediate `10002`) |
| `on_trade_transaction` | 10 | `&Record` | `parse_trade_transaction` (below) |
| `on_book_update` / `on_book_event` | 23 | `&[Record]` / `&BookEvent` | `parse_book_entries` |
| `on_symbol_details` | 17 | `&[Record]` | `SYMBOL_DETAILS` records + `symbol` |
| `on_symbol_update`, `on_login_status` | 13, 15 | `&CommandResult` | raw |
| `on_callback_error` | - | `&CallbackError { command, message }` | called when any callback panics |

### cmd 10: `parse_trade_transaction`

`update_type` is the first u32 (also exposed as `flag_mask`). Layouts are in [protocol.md](protocol.md#11-trade-update-push-cmd-10):

* build 6182, type 2: `transaction_id`, `deals` (one deal), `positions` (one position; `trade_volume == 0` = closed), `balance_info`.
* build 6182, type 0: `transaction_id`, `transaction_type` (0 add / 1 update / 2 delete), `order`; type 1: the same for a history order.
* older pymt5 layouts are still parsed when the length matches them (type 2 with counted lists; 372-byte order pushes).

The client also consumes these pushes itself to maintain the live positions/orders ([client.md](client.md#live-trading-state)); a user handler is optional.

### Writing a safe handler

```rust
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
client.on_tick_event(move |t| { let _ = tx.send(t.clone()); });      // return immediately
tokio::spawn(async move { while let Some(t) = rx.recv().await { /* heavy or async work */ } });
```
