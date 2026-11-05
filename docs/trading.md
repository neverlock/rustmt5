# Trading (`trading.rs`, `order_helpers.rs`, `currency.rs`)

> **Verification status.** Verified against real **demo** accounts (CXMDirect-Demo, build 6182): market open/close, partial close, close-by, pending
> place/modify/cancel (incl. stop-limit and `SPECIFIED` expiration), SL/TP modify, `order_send`/`order_check`, live positions/orders, margin.
> **Never run against a real-money account.** Not verified: instant/exchange execution modes (every symbol checked on the standard USD account is market execution, mode 2), a non-zero `margin_hedged` (the hedged cost term is untested), symbols other than BTCEURs.

### Standard USD demo account (1:2000, hedging) - results

| item | result |
|---|---|
| margin/profit conversions | EURUSD, USDJPY, XAUUSD (margin ccy XAU), EURGBP (profit ccy GBP) all convert to USD; conversion symbols are now auto-subscribed, and the shortest suffixed name wins (`XAUUSDs`, not `XAUUSD247s`) |
| CFD/index/crypto margin | **Verified vs the Web Terminal**: 0.01 lot BTCEURs = 40.37 USD (computed 40.366). Mode 2 margin = `lots * contract * price * margin_rates_initial[buy|sell] * tier`, where `tier` comes from the account's `leverage_rules` (path list matched against `trade.symbol_path`, `*` suffix = prefix; tiers are lot ranges, `margin_rate_initial` applies to the volume inside each range; `C3\Cryptos\*` is 0.5 for the first lot). Account leverage (1:2000) is NOT used for mode 2. Tier handling is per position (MT5 uses total symbol volume) and multi-tier volumes are unverified |
| execution modes | Spec fields live in the nested `trade` record (`info.get_record("trade")`). BTC, US30, AAPL, XAUUSD all report `trade_exemode = 2` (market), `trade_fill_flags = 3`, `volume_min = step = 0.01 lot`. (An earlier note here claiming `exemode = 0` came from reading the top level of the record - wrong.) Instant/exchange modes still not seen on this server |
| stop-limit | wire mapping is `price_order` = **stop (activation) price**, `price_trigger` = **limit price** (buy: limit < stop, else `10015`). Earlier code had the two swapped |
| partial close | position record keeps the id, volume reduced, margin follows |
| close-by | success, but the cmd 10 push has **no position records**; the local state is now updated from the request (`apply_close_by`) |
| SL/TP change | position record (`sl`/`tp`) is updated by the push |
| expiration | `type_time = 2` + `time_expiration` works (rounded to the minute). The server clock was ~7200 s ahead of the local clock, so use server time (e.g. a `time_create` from a position) when computing an expiry |
| `get_account` | current after trades (balance/equity/profit); no cmd 14 push was observed within 3 s |
| tick scaling | prices are real values on this server (EURUSD 1.126) |
| order bookkeeping | finished order ids are remembered so a late/out-of-order push cannot resurrect them in `live_orders` |

## 1. How an order really works on this server

```
trade_request(req)
  1. assign a unique non-zero action_id (if 0)            -> correlation key
  2. register a waiter for that action_id                 -> BEFORE sending
  3. send cmd 12 (248-byte request)                       -> reply: 4-byte ack (0 = accepted)
  4. server pushes cmd 19, several times:  10002 accepted ... final retcode
  5. return when a non-intermediate retcode arrives       -> TradeResult
```

* The cmd 12 ack only says the request was *accepted for processing*. pymt5 treated it as the result and reported `success = true` even for rejected orders.
* Final codes seen: `10009` done, `10008` placed (pending), `10030` invalid filling, `10013` invalid request, `10015` invalid price, `10014` invalid volume.
  `10001`-`10003` are intermediate (`is_intermediate_retcode`).
* If no final push arrives within `config.timeout`, `Error::Timeout("no final trade result push ...")` is returned. **The order may still be live** -
  check positions/orders before retrying.
* Legacy servers that answer cmd 12 with the full 120-byte result (or a non-zero status) are handled without waiting.
* A non-zero command `code` yields `TradeResult { retcode: -2, success: false }`.

`TradeResult { retcode, description, success, deal, order, volume (wire units), price, bid, ask, comment, request_id }`;
`success` is true for retcodes 0, 10008, 10009, 10010.

## 2. The request (cmd 12, 248 bytes)

See [protocol.md](protocol.md#9-trade-request-cmd-12---248-bytes). `TradeRequest` mirrors it (`deviation` is serialised as u32). Pre-send validation (returns
`Error::Validation`, nothing is sent):

| trade_action | requirement |
|---|---|
| DEAL (1), PENDING (5) | `volume > 0` |
| PENDING | `price_order > 0` |
| MODIFY (7), REMOVE (8) | `order > 0` |
| SLTP (6) | `position_id > 0` |
| CLOSE_BY (10) | `position_id > 0` and `position_by > 0` |
| any | `price_sl >= 0`, `price_tp >= 0` |

### `trade_action` is not always DEAL = 1

The Web Terminal chooses the action for **market deals by the symbol's execution mode** (`trade.trade_exemode` from cmd 18) - pymt5 always used 1, which a
market-execution server rejects with `10013`:

| exemode | meaning | action | filling | price_order |
|---|---|---|---|---|
| 0 | request | 1 (0 if no price) | as requested | current price |
| 1 | instant | 2 (falls back to mode 0 above `trade_instant_volume`) | 0 | current price |
| 2 | **market** | **3** | IOC (1) if fill flags & 2, else FOK (0) if & 1, else RETURN (2) | 0 |
| 3 | exchange | 4 | given or RETURN (2) | 0 |

Other fields: new orders use `type_flags = 2`; closes use `type_flags = 3` and `position_id = <ticket>`; pending orders use action 5, `type_flags = 2`, filling RETURN (2)
(FOK/IOC give `10030`); `type_reason` is set from `magic` (the Web Terminal forces 0 - untested whether servers care).
Prices are normalised with `normalize_price(price, tick_size, digits)` (round to tick size, then to digits).

Cancel, modify and SLTP **resend the existing object's data** (symbol, volume, order type, prices), so they first look the order/position up. A bare
"cancel order N" request is rejected with `10013`.

## 3. Order helpers (`order_helpers.rs`)

```rust
use rustmt5::{CloseOptions, MarketOrderOptions, ModifyOrderOptions, PendingOrderOptions};

client.buy_market("EURUSDc", 0.01, &MarketOrderOptions { sl: 1.10, ..Default::default() }).await?;
client.sell_limit("EURUSDc", 0.01, 1.20, &PendingOrderOptions::default()).await?;
client.buy_stop_limit("EURUSDc", 0.01, /*stop trigger*/ 1.13, /*limit*/ 1.129, &Default::default()).await?;
client.modify_position_sltp(position_id, Some(1.10), None).await?;      // None keeps the current level
client.close_position(position_id, None, &CloseOptions::default()).await?; // None = whole volume
client.modify_pending_order(order, &ModifyOrderOptions { price: Some(1.05), ..Default::default() }).await?;
client.cancel_pending_order(order).await?;
```

| helper | action | notes |
|---|---|---|
| `buy_market`/`sell_market` | by exemode (table above) | reference price = `options.price`, else cached tick (ask for buy, bid for sell) |
| `buy_limit`/`sell_limit`/`buy_stop`/`sell_stop` | 5 | |
| `buy_stop_limit`/`sell_stop_limit` | 5 | `price` = stop (activation) price -> `price_order`; `stop_limit_price` -> `price_trigger` (buy: limit < stop, else `10015`; verified live) |
| `close_position(id, volume, opts)` | by exemode | looks the position up (side, symbol, current price); opposite `trade_type`, `type_flags = 3` |
| `close_position_by(id, by)` | 10 | both positions must be of the same symbol and opposite |
| `modify_position_sltp(id, sl, tp)` | 6 | `None` keeps, `Some(0.0)` removes |
| `modify_pending_order(order, opts)` | 7 | unspecified fields keep the current value |
| `cancel_pending_order(order)` | 8 | |

Defaults: `deviation 0`, `filling None` (auto), `digits None` (from symbol info), `time_type GTC`. `find_position`/`find_order` retry up to 4 times (1 s apart) and then
return `Error::Validation("position N not found")`.

## 4. Live positions and orders

`get_positions_and_orders()` / `get_positions()` / `get_orders()` return the **live state**: the cmd 4 snapshot taken at login, kept current by cmd 10 pushes
(rules in [client.md](client.md#live-trading-state)). Reasons:

* On build 6182 the server answers cmd 4 with the *login-time snapshot* for the whole session: a position opened later never appears, a closed one never disappears.
* A freshly opened position appears in the live state as soon as the type-2 push arrives (~immediately on the demo server).

`profit`, `price_close` etc. of a position are the values of the last snapshot/push, **not** repriced continuously (`account_margin` reprices floating profit from the tick cache).
Other helpers: `positions_total/orders_total`, `positions_get(symbol|group|ticket)` (the **first given filter wins**), `orders_get`, `history_orders_get/history_deals_get`
(`HistoryFilter { group, ticket, position }`, AND-combined), `get_trade_history(from, to)` (deals' `time_create/time_update` are converted to milliseconds, other records stay in seconds).
`reload_positions_and_orders()` discards the live state and reseeds it from a new cmd 4 answer (only meaningful on servers with current cmd 4 answers).

## 5. Margin, equity and profit (local)

The cmd 3 account record has **no margin fields**, and the pushes' balance block does not hold used margin (the six values were `[balance, 0, 0, 0, x, 0]` where `x` did not change
between open and close). The Web Terminal computes margin client-side, so does this library:

`account_margin()` -> `{ margin, equity, profit (floating), margin_free, margin_level, positions, unpriced, margin_source = "computed" }`

* per position: `margin += calc_margin_raw(symbol info, lots, price_open, leverage) * rate_margin`; `rate_margin` is the margin-currency -> account-currency rate **supplied on every
  position record** (e.g. EUR -> USC = 112.64); if absent a tick-based conversion is used (`resolve_conversion_rates`).
* floating profit: re-priced from the cached tick (`calc_profit_raw * rate_profit`) when a tick and `rate_profit` exist, else the position's last reported `profit`.
* `equity = balance + credit + floating profit`; `margin_level = equity / margin * 100`.
* **Not modelled:** hedged-margin offsets (opposite positions count in full), swap/commission in equity. `unpriced > 0` means some position could not be priced (margin missing).
* Verified live: 0.01 lot EURUSDc on a 1:2000 USC account -> `0.01 * 1000 / 2000 * 112.636 = 0.5632`, matching the computed value.

`get_account_summary()` and `order_check` use these figures.

### Formulas (`currency.rs`)

Profit (`calc_profit_raw`, in the symbol's profit currency, before conversion):

| calc_mode | formula | needs (else `last_error`) |
|---|---|---|
| 37, 39 (bond) | `dir*((pc/100*face + accrued) - po/100*face)*contract*vol` | contract, face_value (-10) |
| 64 (collateral) | 0 | |
| 0, 5 (forex), 2, 3, 4, 32, 38 (CFD), other | `dir*(pc - po)*contract*vol` | contract_size (-11) |
| 1, 33, 35, 36 (futures/options) | `dir*(pc - po)*vol*pv` | tick_value (-12) |
| 34 (FORTS) | `dir*(f(pc) - f(po))*vol`, `f(p) = p*tick_value/tick_size` | tick_value (-12) |

`dir = +1` for buy, `-1` for sell; `pv = tick_value/tick_size` (or `tick_value` when `tick_size = 0`).

Margin (`calc_margin_raw`):

| calc_mode | formula | error |
|---|---|---|
| 37, 39 (bond) | `lots*contract*face*price/100` | -13 |
| 64 | 0 | |
| 5 | `lots*contract` | -14 |
| 0 (forex) | `lots*contract / leverage` | -14, -15 |
| 4 (CFD leverage) | `lots*contract / leverage * price` | -14, -15 |
| 1, 33, 34 (futures) | `margin_initial > 0 ? lots*margin_initial : lots*price*pv` | -16 |
| 3 (CFD index) | `lots*contract*price*pv` | -17 |
| 2, 32, 35, 36, 38 | `(mode in {32,35,36,38} && margin_initial > 0) ? lots*margin_initial : lots*contract*price` | -14 |
| other | - | -18 |

### Currency conversion (`resolve_conversion_rates`)

1. Cent currencies are converted through their main currency and scaled (`USC = USD/100`, also `EUC`, `GBC`, `AUC`).
2. Same currency (`RUB` == `RUR`) -> (1, 1).
3. Direct pair `"{src}{dst}"`; else a name starting with `src` and ending with `dst`; else (new) a name starting with `"{src}{dst}"` (broker suffix, e.g. `EURUSDc`).
4. Prices come from the tick cache; the current symbol falls back to the caller's price. Inverse pairs use `1/bid` or `1/ask`; otherwise triangulate via USD.
5. Direct pairs: buy uses ask, sell uses bid.

Applied: profit in forex modes `raw * (buy ? rate_buy : rate_sell)`, other modes `raw * (raw > 0 ? rate_sell : rate_buy)`; margin `raw * (buy ? rate_buy : rate_sell)`.
The conversion pair needs a **tick subscription** (or be the symbol itself); otherwise `-5`/`-6`. Return type is `Result<Option<f64>>`: `Ok(None)` + `last_error()` for ordinary failures.

## 6. `order_send` - MetaTrader5-style dict

`order_send(&Record)` keys: `action`, `symbol`, `volume` (lots), `type`, `price`, `stoplimit`, `sl`, `tp`, `deviation`, `comment`, `position`, `position_by`, `order`, `type_filling`,
`type_time`, `expiration`, `magic`. It **dispatches to the helpers above**, so the right wire representation is chosen:

| `action` | becomes |
|---|---|
| DEAL (1), no `position` | `buy_market` / `sell_market` (by `type`) |
| DEAL (1), with `position` | `close_position(position, volume or whole)` |
| PENDING (5) | pending order of `type` (stop-limit: `price` = stop/activation price, `stoplimit` = limit price) |
| SLTP (6) | `modify_position_sltp(position, sl, tp)` (keys absent = keep) |
| MODIFY (7) | `modify_pending_order(order, ...)` |
| REMOVE (8) | `cancel_pending_order(order)` |
| CLOSE_BY (10) | `close_position_by(position, position_by)` |
| other | `Error::Validation` |

`trade_request(&TradeRequest)` remains the raw interface (no translation, no lookups).

## 7. `order_check` - local pre-flight

Not a server call: it checks the rules it knows from symbol/account data. "OK" does not guarantee the server accepts the order.

```
normalized = normalize_order_request(request)
(retcode, comment) = validate_order_check_request:
    DEAL/PENDING: validate_deal_or_pending (list below); SLTP/MODIFY/REMOVE/CLOSE_BY: required ids present; other: 10013
account = get_account();  am = account_margin_from(account)          // margin/equity/profit computed locally
if retcode in {0, 10008, 10009, 10010}: additional = estimate_order_margin()   // Option
margin_post = am.margin + additional ; margin_free = am.equity - margin_post
margin_level = am.equity / margin_post * 100  (0 if margin_post <= 0)
if still OK and margin_free < 0 -> retcode = 10019 "Not enough money"
```

`validate_deal_or_pending`, first failure wins: empty symbol / unknown symbol -> 10013; `volume <= 0` -> 10014; unknown order type -> 10035; `trade_mode`
0 -> 10017, 1 (long only) with a sell -> 10035, 2 (short only) with a buy -> 10035, 3 (close only) -> 10017; **volume vs min/max/step** -> 10014
(after converting the symbol's wire volumes to lots with `symbol_info_volumes_in_lots`); SPECIFIED expiration without `expiration` -> 10022; filling not in {0,1,2}
when the symbol has fill flags -> 10030; no reference price -> 10015; SL/TP/trigger closer than `point * trade_stops_level` -> 10016.

Result keys: `retcode, balance, equity, profit, margin, margin_free, margin_level, comment, margin_known, request`.
`margin_known = false` when the margin of the order could not be computed (no price or no conversion rate) - then a "passing" result is inconclusive.
Parse/validation errors in the middle give `retcode = -99` with the account figures.

Verified live (EURUSDc, demo): 0.01 lot -> OK with margin 0.5632; 0.005 -> 10014 "below symbol minimum 0.01"; 0.015 -> "does not align with step 0.01";
100000 -> "above symbol maximum 1000".

### Known limitations

* No freeze-level, pending-order-count, trading-session or hedged-margin checks.
* `tick_size` was 0 on the tested broker; futures/FORTS formulas then use `tick_value` directly (may differ elsewhere).
* Volume precision is fixed at `10^8` (`VOLUME_PRECISION`), confirmed on the tested servers only.
* After a trade, the live state follows pushes; if a push type is missed (for example an unobserved SL/TP-change push), `reload_positions_and_orders()` or a new login corrects it.
