# Handoff - rustmt5

Read this first in a new session. It tells you where the project stands and what to do next. Details live in [`docs/`](docs/README.md).

## 1. What this is

`rustmt5` is a Rust port of [cloudQuant/pymt5](https://github.com/cloudQuant/pymt5): an async client for the MetaTrader 5 **Web Terminal** binary WebSocket protocol.
Repo: `neverlock/rustmt5`, default branch **`master`** (the branch `claude/bold-cerf-guqm8b` mirrors it and can be deleted on GitHub; pushes of deletions are blocked from the cloud session).

State at handoff (commit `b475c9c` or later): ~40 unit tests + 9 integration tests (mock server) + 1 doctest pass, `cargo clippy --all-targets -- -D warnings` is clean.
Verified against real servers (CXMDirect, build 6182): connect, handshake, login, account, symbols, groups, bars, ticks, full symbol info, history, **trading on a demo account**
(market open/close, SL/TP, pending place/modify/cancel, `order_send`, `order_check`, live positions, local margin). See the status tables in
[`docs/reference/commands.md`](docs/reference/commands.md) and [`docs/trading.md`](docs/trading.md).

## 2. Environment variables (cloud environment settings, read at session start)

Set in the cloud environment menu (title bar -> Edit); **a new session is needed after any change**. Never print or commit values; compare lengths only.

| variable | account |
|---|---|
| `LOGIN`, `PASSWORD`, `SERVER` | **real-money** account, read-only use only (`live_check`). Password length is 8 with a `#` suffix problem history: a `#` in an env value can truncate it. |
| `DEMO_LOGIN`, `DEMO_PASSWORD`, `DEMO_SERVER` | demo account in **cent currency (USC)**, 1:2000, hedging |
| `DEMO_STD_LOGIN`, `DEMO_STD_PASSWORD` (+ optional `DEMO_STD_SERVER`, else use `DEMO_SERVER`) | demo **standard USD** account - **was not visible in the last session; check first** |

First thing to do: `env | cut -d= -f1 | grep -i -E "demo|std"` (names only). If `DEMO_STD_*` are still missing, tell the user and stop trading tests.

## 3. Hard safety rules

1. **Trade only on accounts where `get_account().is_demo == true`** (the `demo_trade_check` example refuses otherwise). Never send orders to the real account.
2. A login failing with `code=3` twice: stop and check the credentials (account lock risk). Do not loop.
3. Clean up after every test: no leftover positions/orders (check with a fresh login: the login snapshot is accurate).
4. Never print passwords; before committing run `grep -rIF "$PASSWORD" .` and the same for the `DEMO_*` passwords - nothing may match.
5. Do not execute third-party code from the pymt5 repo (the generators in `tools/` read it with `ast` on purpose). Do not scan other processes' environments.
6. Do not create PRs (the user wants direct pushes to `master`).

## 4. Next tasks (in order)

### A. Verify on the standard USD demo account (main open item)
Run `cargo run --example demo_trade_check` with `DEMO_LOGIN/PASSWORD/SERVER` pointed at the USD account (the example reads `DEMO_*`; either export the `DEMO_STD_*` values into `DEMO_*` for the command
or extend the example to accept a `DEMO_STD_` prefix) and check each of the following; fix code and add tests/docs for anything that differs:

1. Non-cent conversions: `order_calc_margin`/`order_calc_profit`/`order_check` on EURUSD-like, USDJPY-like, XAUUSD-like and a non-USD-quoted cross (EURGBP-like). Compare `account_margin()` with the broker's
   own numbers (the Web Terminal shows margin/free margin; if the user can read them, ask for a screenshot/values).
2. Which symbols/execution modes exist: print `trade.trade_exemode`, `trade_fill_flags`, `trade_instant_volume`, `volume_min/step` for forex, metals, crypto, stocks, indices. Exercise a symbol with
   instant (1) or exchange (3) execution if available - the code paths exist (`apply_execution_mode`) but are only unit-tested.
3. Stop-limit orders (`buy_stop_limit`): check `price_trigger`/`price_order` mapping and the pushes.
4. Partial close (`close_position(id, Some(0.5*volume), ..)`) - check the position push (volume reduced, not removed) and the margin afterwards.
5. Hedged close-by (`close_position_by`): needs two opposite positions of one symbol on a hedging account.
6. SL/TP change: which push arrives (type 2? type 0?) and whether the live position record shows the new `sl`/`tp`. If no push updates it, patch `apply_trade_update` or refresh the record after the helper.
7. Pending order expiration (`type_time` SPECIFIED) and `ORDER_TIME_*` handling.
8. Account push cmd 14 / balance changes after a trade: is `acc_profit`/equity in cmd 3 current? Compare `get_account()` after opening a position.
9. Unknown-server differences: tick price scaling (`normalize_ticks`) on this server.

Record results in `docs/trading.md` (verification status) and `docs/reference/commands.md`.

### B. Improvements already identified (see `docs/contributing.md` section 6)
* Hedged-margin offsets in `account_margin` (opposite positions of one symbol).
* Re-price positions continuously (profit/`price_close` are as of the last push); maybe maintain from ticks.
* `on_reconnect_failed` callback after the last reconnect attempt.
* HTTP CONNECT proxy support for the WebSocket (currently direct TCP).
* Detect volume precision per server instead of the fixed `10^8` (`VOLUME_PRECISION`).
* Decide what to do with the unused `Error::Trade` (wrapper that turns unsuccessful `TradeResult` into an error?).
* Optional: make `demo_trade_check` take the symbol list / account prefix from arguments; add a `live_check`-style read-only example for the demo accounts.

### C. Housekeeping
* The user may delete the stale branch `claude/bold-cerf-guqm8b` on GitHub.
* Keep `docs/` in **English**.
* If you change a wire layout, add a unit test pinning the observed bytes and update `docs/reference/schemas.md` (its "Corrections" section lives in `tools/gen_schema_docs.py`).

## 5. How to work here

```bash
cargo test --all-targets && cargo clippy --all-targets -- -D warnings
cargo run --example live_check          # read-only, uses LOGIN/PASSWORD/SERVER (real account) - consider using DEMO_* instead
cargo run --example demo_trade_check    # demo only; uses DEMO_LOGIN/DEMO_PASSWORD/DEMO_SERVER, SYMBOL=EURUSDc by default
```

Debug scratch scripts go in `examples/scratch_probe.rs` and **must be deleted before committing** (a stop hook complains about untracked files).
Commit messages end with the attribution lines from the session; push with `git push origin HEAD:refs/heads/master` (and `git push origin claude/bold-cerf-guqm8b` to keep the hook quiet).

## 6. Hard-won facts (so you do not rediscover them)

* The cmd 12 request is **248 bytes** (pymt5's 236 is wrong); the answer is only an ack, results come as cmd 19 pushes with the same `action_id`; `10002` is intermediate.
* Market deals use `trade_action` by execution mode (mode 2 -> action 3, IOC if allowed); pending orders need filling RETURN(2); close = `type_flags 3` + position ticket; cancel/modify resend the order's data.
* cmd 4 is a **login-time snapshot**; positions/orders are maintained from cmd 10 pushes (type 2 = deal+position+balance, closed position has `trade_volume 0`; type 0/1 = orders).
* The server never sends used margin; it is computed locally using `rate_margin`/`rate_profit` carried by each position.
* cmd 18 takes **symbol ids**, not names. `volume_min/max/step` are wire volumes (`lots * 10^8`).
* The demo server sends scaled-integer tick prices (`112623` = 1.12623); the live server sends real prices -> `ClientConfig::normalize_ticks`.
* TLS inside the cloud sandbox needs `SSL_CERT_FILE` (already set there); the library adds that CA bundle to rustls' roots.
* A `#` inside an env value can silently truncate a password -> login `code=3`.
