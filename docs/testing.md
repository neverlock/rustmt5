# Testing

## 1. Running

```bash
cargo test --all-targets          # 40 unit tests + 9 integration tests against the mock server
cargo test --doc                  # doctest
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps
```

CI (`.github/workflows/ci.yml`) runs clippy (warnings denied), the tests and the doctest.

## 2. Unit tests (`#[cfg(test)]` modules)

| file | covers |
|---|---|
| `helpers.rs` | hex, UTF-16LE round trip (incl. Thai text), ASCII padding, client id |
| `crypto.rs` | 32-byte initial key, round trip for all key sizes, bootstrap packet sizes equal the browser capture |
| `protocol.rs` | schema sizes (526/50/48/344/356/356), outer/response frames, FILETIME, round trips, out-of-range integers |
| `parsers.rs` | glob and group masks, timestamps, tick normalisation (with and without), tick cache merge, bars incl. the ambiguous 336-byte case, counted records, order book, volume/stops validation incl. wire-to-lots conversion, open-account results, account response, **trade result push v2**, **cmd 10 push layouts (v2, 736 B, legacy)** |
| `push_handlers.rs` | live position/order state following pushes (open, resize, close, placed, cancel requested, deleted, history) |
| `order_helpers.rs` | price normalisation, `trade_action`/filling per execution mode |
| `currency.rs` | cent currency split |
| `validation.rs`, `rate_limiter.rs`, `order_manager.rs`, `account.rs` | as named |

Some tests deliberately pin pymt5 behaviour (e.g. `2000.0` with `digits = 2` normalises to `20.0`; the empty trade-settings quirk).

## 3. Integration tests with the mock server (`tests/mock_server.rs`)

The mock runs in-process (`TcpListener` + `tokio_tungstenite::accept_async`) and speaks the real protocol: it decrypts with the initial key, answers the bootstrap
with a session key and then switches ciphers. A `match cmd` serves:

| cmd | answer |
|---|---|
| 0 | `build 6182 | token | key` |
| 28 | checks the payload is 912 B; token + session 777, or `code 3` (`reject_login`) |
| 3 | account (balance 1000.5, USD, 1:100, demo) with one trade-settings record |
| 34 | two symbols, zlib compressed |
| 7 | ack, then two tick pushes (scaled prices; the second has `bid = 0`) |
| 18 | requires `[n][ids]` (else `code = 1`, like build 6182); full symbol with `trade_exemode 2`, fill flags 3, wire volume limits |
| 4 | one position (id 900) and one order (id 901) plus the 12-byte trailer |
| 12 | checks the 248-byte layout; validity rules of a market-execution server (see below); 4-byte ack then cmd 19 push(es) |
| other | empty ack; `ignore_cmd` = no answer (timeouts) |

cmd 12 validity rules in the mock: `action 3` with `type_flags 2` and filling 0/1 (open) or `type_flags 3` with a position (close); `5` with filling 2 and flags 2; `6` with a position;
`7`/`8` with an order and volume; `10` with both positions; filling `7` -> `10030`; anything else (including the old pymt5 style `action 1`) -> `10013`.
`Behavior.trade_mode`: 0 = ack + push (6182 style), 1 = legacy full response, 2 = ack only (timeout test). `Behavior.requests` records every cmd 12 payload so tests can assert field offsets.

| test | proves |
|---|---|
| `handshake_login_ping_logout` | encrypted handshake, login, ping, health, close |
| `login_rejection_is_authentication_error` | `code = 3` -> `Error::Authentication` |
| `account_symbols_ticks_and_trade` | account/symbol/tick parsing, subscribe merging, managed handle, full symbol, local margin, `order_check` units/margin/no-money, trade results incl. async rejection and typed events |
| `order_helpers_build_web_terminal_requests` | action/filling/flags per helper, lookups for close/cancel/modify/SLTP, `order_send` dispatch, old style rejected |
| `trade_result_legacy_immediate_and_timeout` | legacy servers; ack without push -> `Error::Timeout` |
| `command_timeout_and_transport_not_ready` | `Error::Session` before connect; `Error::Timeout`, connection stays usable |
| `disconnect_callback_and_auto_reconnect_restores_subscriptions` | drop -> callback -> reconnect -> re-login -> listeners still attached |
| `panicking_push_callback_does_not_kill_receive_loop` | panics are contained |
| `login_payload_matches_browser_capture_sizes` | 916/928/936 byte frames |

### Extending the mock

1. Add an arm to `serve_connection` (reply with `frame(&cipher, cmd, code, &body)`; the reply **must use the request's command id**).
2. Build bodies with `record_bytes(SCHEMA, NAMES, &[("field", Value::...)])` (unspecified fields are zero/empty).
3. For pushes, `ws.send` additional frames after the reply.

## 4. Live and demo checks

All read-only unless stated; **none prints the password**.

```bash
# read-only connectivity check (login, account, symbols, groups, positions, history, bars, ticks, health)
LOGIN=... PASSWORD=... SERVER=wss://host/terminal cargo run --example live_check

# trading round trip on a DEMO account (refuses non-demo accounts): market open, SL/TP, close, pending place/modify/cancel
DEMO_LOGIN=... DEMO_PASSWORD=... DEMO_SERVER=wss://host/terminal SYMBOL=EURUSDc cargo run --example demo_trade_check
```

### Lessons from live testing (read before debugging "protocol" problems)

* A login `code=3` repeated: **stop retrying** (account lock risk) and check the credentials. A `#` in the value of an env var/`.env` file often truncates it.
* Environment variables are read when the process/session starts; changing them later has no effect on a running session.
* Behind a TLS-intercepting proxy set `SSL_CERT_FILE` or `ClientConfig::ca_bundle` (never disable verification).
* No ticks = market closed; test with a 24/7 crypto symbol.
* cmd 4 results are stale after login; use the live state ([trading.md](trading.md)). Right after a trade allow a moment for the push.
* Different servers differ: the demo server sends scaled-integer ticks, the live one real prices; account currencies can be cents (USC).
* Always clean up after demo tests (close positions, cancel orders); the examples do.
* Never commit credentials; before committing run `grep -rIF "$PASSWORD" .` (it must find nothing).

## 5. Not yet verified against a real server

Partial close, close-by (hedging), stop-limit orders, instant/exchange execution modes, DOM, push cmd 13/14/15/17, spreads, account opening, password change, OTP, cmd 6,
cmd 52, and a standard USD account (a USD demo account is planned; see [contributing.md](contributing.md#6-open-work)). Per-command status: [reference/commands.md](reference/commands.md).
