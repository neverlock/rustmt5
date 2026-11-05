# Client core (`src/client.rs`)

## 1. `ClientConfig`

| field | default | meaning |
|---|---|---|
| `uri` | `wss://web.metatrader.app/terminal` | WebSocket URL |
| `timeout` | 30.0 | seconds, for connecting and for every command (also the wait for a trade result) |
| `heartbeat_interval` | 30.0 | seconds between heartbeat pings |
| `tick_history_limit` | 10 000 | ticks kept per symbol (0 = unlimited) |
| `max_tick_symbols` | 0 | symbols with tick history (0 = unlimited; beyond that the least recently updated is evicted) |
| `auto_reconnect` | false | reconnect automatically |
| `max_reconnect_attempts` | 5 | |
| `reconnect_delay` / `max_reconnect_delay` | 3.0 / 60.0 | backoff base and cap |
| `rate_limit` / `rate_burst` | 0 / 20 | token bucket |
| `metrics` | None | |
| `symbol_cache_ttl` | 0 | seconds; 0 = `load_symbols` always refetches |
| `ca_bundle` | None | extra PEM root CAs (see [transport.md](transport.md)) |
| `normalize_ticks` | true | undo the scaled-integer tick price heuristic; set `false` when the server already sends real prices ([market-data.md](market-data.md)) |

`ClientConfig::new(uri)` sets the URI and keeps the other defaults.

## 2. Internal `State`

`Inner.state: Arc<Mutex<State>>` holds:

| field | used for |
|---|---|
| `symbols`, `symbols_by_id`, `symbols_loaded_at` | `load_symbols` cache; name <-> id; tick normalisation |
| `full_symbols` | cmd 18 results per name (cleared when `load_symbols` reloads) |
| `tick_cache_by_id/name`, `tick_history_by_id`, `tick_history_access_order` | latest ticks, merge of unchanged prices, `copy_ticks_*`, LRU |
| `book_cache_by_id/name` | latest DOM |
| `subscribed_ids`, `subscribed_book_ids` | current subscriptions, re-sent on reconnect |
| `live_positions`, `live_orders`, `trade_state_ready` | live trading state (below) |
| `trade_waiters`, `next_action_id` | correlation of `trade_request` with its cmd 19 pushes |
| `logged_in`, `bootstrap_pristine`, `closing`, `login_params` | session flags and stored credentials |
| `last_error`, `reconnect_count`, `connected_at`, `health_threshold_ms`, `normalize_ticks` | misc |

pymt5 keeps two tick-history maps (by id and by name sharing one deque); Rust keeps one keyed by id and finds it by name through `symbols`.

### Live trading state

`login_with` resets `live_positions`/`live_orders` and, after a successful login, calls `get_positions_and_orders()` once to seed them from the
server's cmd 4 snapshot (taken at login). From then on cmd 10 pushes update them (`apply_trade_update` in `push_handlers.rs`):

* `update_type 2`: for every POSITION in the push, `trade_volume == 0` removes it, otherwise it is inserted/replaced.
* `update_type 0`: ORDER with `transaction_type == 2` or a non-live `order_state` (live = 0, 1, 3, 7, 8) is removed; otherwise inserted/replaced.
* `update_type 1` (history order): the order is final and is removed.

`get_positions_and_orders()` returns this live state (it still sends cmd 4 so the first call seeds it). `reload_positions_and_orders()` discards the
state and reseeds from a new cmd 4 answer - useful only on servers whose cmd 4 answers are current.

## 3. Login flow

```
connect()          Transport::connect; bootstrap_pristine = true; connected_at = now
[init_session()]   cmd 29 (optional); bootstrap_pristine = false
login(login, pw)   = login_with(LoginParams::new(login, pw))
   - reset live state; build the 912-byte payload; send cmd 28
   - code != 0 -> Err(Authentication("login rejected by server: code=N ..."))
   - parse (token, session_id); logged_in = true; store LoginParams (for reconnect)
   - auto_heartbeat -> start the heartbeat task
   - seed the live trading state (cmd 4), failures are only logged
```

`LoginParams` has a custom `Debug` that **redacts the password** - keep it that way. `is_connected()` = `transport.is_ready() && logged_in`.

### `bootstrap_pristine` and cmd 52

On build 5687 `cmd 52` is only safe before init/login (otherwise the server drops the socket). The flag is set after `connect` and cleared by
`init_session`, `login` and `send_raw_command(28|29|52)`; `send_bootstrap_command_52` refuses when it is not pristine.

## 4. Heartbeat

`start_heartbeat` spawns a task: `sleep(heartbeat_interval)` then `ping()` (failures are logged as warnings). It holds a `Weak<Inner>`, so it ends
when all clients are dropped; `stop_heartbeat` aborts it on logout/close/disconnect.

## 5. Reconnect

Starts when the transport reports a disconnect, `auto_reconnect` is on, `close()` is not running and credentials are stored:

```
handle_disconnect(): logged_in = false | stop heartbeat | call the user's on_disconnect | spawn reconnect_loop (if none is running)

reconnect_loop():
  for attempt in 1..=max_reconnect_attempts:
      delay = min( base * 2^(attempt-1) + U(0, base), max_delay )
      metrics.on_reconnect_attempt; sleep(delay)
      try: transport.close(); transport.connect()
           login_with(stored params, auto_heartbeat = true)         // also reseeds the live trading state
           subscribe_ticks(subscribed_ids); subscribe_book(subscribed_book_ids)
           reconnect_count += 1; metrics.on_reconnect_success; return
      on error: log and try the next attempt
  exhausted: log an error (there is no "gave up" callback yet)
```

**Difference from pymt5:** the same `Transport` is reused, so listeners (the client's caches and user handlers) survive. pymt5 creates a new transport
and silently loses them (covered by the integration test `disconnect_callback_and_auto_reconnect_restores_subscriptions`).
Commands in flight when the socket drops fail immediately with `Error::Connection`.

## 6. `close()`

1. `closing = true` (suppresses reconnect), stop the heartbeat, abort the reconnect task.
2. If logged in: `logout()` (errors ignored).
3. `transport.close()` (in-flight commands fail with `Error::Session("transport closed")`).
4. Drop `login_params` (the stored credentials), `closing = false`.

## 7. Health

`health_check()` returns `HealthStatus { state, ping_latency_ms, last_message_age_secs, uptime_seconds, reconnect_count }`. When ready it pings to measure
latency; if latency exceeds the threshold (`on_health_degraded(cb, threshold_ms)`, default 5000) the callbacks run.

## 8. `last_error` (MetaTrader5 compatibility)

"Soft failing" official-style helpers (`order_calc_profit`, `order_calc_margin`, `order_check`, `version`) return `None`/a retcode and store `(code, message)`
readable with `last_error()`: `-1` unsupported action, `-2` symbol not found, `-3` no account currency, `-4` no `currency_profit`, `-5`/`-6` cannot convert
profit/margin currency, `-7` no build number, `-10..-18` insufficient symbol data for the formulas ([trading.md](trading.md)), `-99` parse/validation error.

## 9. Escape hatches

* `send_raw_command(cmd, payload)` - any command in `VALID_COMMANDS` (handy for exploring new commands).
* `transport()` - raw `on/off` listeners.
