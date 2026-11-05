# Market data (`market_data.rs`, `parsers.rs`, `push_handlers.rs`)

## 1. Symbols

### `load_symbols(use_gzip)`

```
get_symbols(true)  -> cmd 34 -> body = [4 B header][zlib stream]
                      body.len() <= 4 -> []                         (no data)
                      zlib decompress (spawn_blocking) -> [u32 n][SYMBOL_BASIC 526 B]*
get_symbols(false) -> cmd 6  -> [u32 n][SYMBOL_BASIC]*
```

It then clears `symbols`, `symbols_by_id`, `full_symbols` and rebuilds `SymbolInfo` values. With `symbol_cache_ttl > 0` a still-valid cache is returned without
asking the server (`invalidate_symbol_cache()` forces a refetch).

Verified live: 968 symbols and 13 groups (live) and 923 symbols (demo). Group names use `\` as separator, e.g. `C3\FX MajorsP`. Brokers add suffixes to
names (`EURUSDp`, `EURUSDc`): use the names the server sends (`symbol_names()`), never guess them.

### Lookup helpers

| method | does |
|---|---|
| `get_symbol_info(name)`, `get_symbol_id(name)`, `symbol_names()` | read the cache (no server call) |
| `resolve_symbol_id(name)` (internal) | cache, else `load_symbols`, then look again |
| `symbols_get(group, gzip)` | always refetches, filters with a glob mask |
| `symbols_total` | cache size, else fetch and count |

### Group mask (`parsers::matches_group_mask`)

Comma separated; entries starting with `!` exclude; glob syntax like Python's `fnmatchcase` (`*`, `?`, `[abc]`, `[a-z]`, `[!x]`, **case sensitive**).
`"EUR*,GBP*,!*JPY"`. An empty mask matches everything; a mask with only exclusions means "everything except".

## 2. Full symbol info (cmd 18) - contract specification

```rust
client.get_full_symbol_info("EURUSDc").await?   // Option<Record>
client.get_full_symbols(&[id1, id2]).await?     // RecordList
```

* **The request is `[u32 count][u32 symbol_id]*`, not a name.** pymt5 sends the name, which build 6182 rejects with `code=1` and an empty body. The method resolves
  name -> id first (loading symbols if needed) and falls back to the name request for older servers.
* Response: `[u32 count][FULL_SYMBOL 3068 B]*`. The last three fields are nested binary blocks that are parsed:
  `trade` (628 B, `ACCOUNT_WEB_TRADE_SETTINGS`), `schedule` (896 B: 7 days x 16 sessions x (u16,u16), quote sessions then trade sessions) and
  `subscription` (8 B: delay u32, status u8, level u8, reserved u16).
* `normalize_full_symbol_record` copies fields of `trade` to the top level so it reads like MetaTrader5:

  | top-level key | from `trade.` |
  |---|---|
  | `symbol_path`, `trade_mode`, `trade_stops_level`, `trade_freeze_level` | same name |
  | `volume_min/max/step/limit`, `margin_initial`, `margin_maintenance` | same name |
  | `filling_mode`, `expiration_mode`, `order_mode` | `trade_fill_flags`, `trade_time_flags`, `trade_order_flags` |
  | `trade_face_value`, `trade_accrued_interest` | top-level `face_value`, `accrued_interest` |

* **`volume_min/max/step/limit` are wire volumes** (0.01 lot = 1 000 000); convert with `parsers::symbol_info_volumes_in_lots` before comparing with lots.
* The execution fields used for trading live in the nested `trade` record: `trade_exemode`, `trade_fill_flags`, `trade_instant_volume`.
* `point` is top-level; `tick_size` was **0** on the tested broker (formulas handle `tick_size = 0`).
* Results are cached in `State.full_symbols` until the next `load_symbols`. `code != 0` or an empty body yields `None` (not an error).

Verified live on 9 symbols (forex, gold, crypto, stocks): e.g. EURUSDc `contract_size 1000`, `trade_exemode 2`, `trade_fill_flags 3`, stops level 0.

`symbol_info(name)` = cache -> cmd 18 -> basic `SymbolInfo` fields.

## 3. Ticks

### Subscribing

```rust
client.subscribe_ticks(&[id]).await?;                 // merged with the existing set, sorted, deduplicated
client.unsubscribe_ticks(&[id]).await?;               // sends the remaining set; empty sends [0] (stop all)
client.subscribe_symbols(&["EURUSDc"]).await?;        // by name; unknown name -> Error::SymbolNotFound
client.subscribe_symbols_batch(&[...]).await?;        // silently skips unknown names
let mut h = client.subscribe_ticks_managed(&ids).await?;  h.unsubscribe().await?;
```

cmd 7 **replaces** the server-side set, so the full merged set is always sent; `State.subscribed_ids` keeps it for reconnects.

### Tick push -> `Record`

`TICK_SCHEMA` (50 B): `symbol_id, tick_time (s), fields, bid, ask, last, tick_volume, time_ms_delta, flags`, plus `tick_time_ms = tick_time*1000 + time_ms_delta`
and `symbol` when the id is known.

**Partial updates:** unchanged prices arrive as `0.0`. `parse_tick_batch(body, symbols_by_id, cache, normalize)` fills a zero `bid/ask/last` from the cached
previous tick of the same symbol. The cache used is the one from *before* the batch, so values are not carried across ticks inside one batch (as in pymt5).
The client's cache listener runs before user handlers, so they see merged values.

### Price normalisation (`normalize_tick_price_value`) - a heuristic

Some servers send prices as integers scaled by `10^digits`. **Observed: the CXMDirect demo server does (`112623` = 1.12623); the live server sends real prices
(`80709.1`).** The heuristic:

```
digits == 0 or price == 0                  -> unchanged
forex-like = first 6 chars alphabetic (EURUSD)
threshold  = 10^digits ; if digits>=4 && forex-like && no "JPY" -> 10^digits / 10
forex-like && digits>=3  -> |price| >= threshold ? price/10^digits : price
otherwise                -> non-integer (|p-round(p)| > 1e-9)  -> unchanged
                            |round(p)| < threshold             -> unchanged ; else round(p)/10^digits
```

**Known risk:** the last rule treats an integer price `>= 10^digits` as scaled, so a stock at `200.00` (digits 2) becomes `2.0` and gold at `2000.0` becomes
`20.0` (unit-tested to match pymt5). Use `ClientConfig::normalize_ticks = false` on servers that already send real prices (the live CXMDirect server);
keep it `true` for servers that scale (the demo server). Mixed environments need one client per server.

### Caches and history (installed in `Mt5Client::new` -> `install_cache_listeners`)

For every parsed tick: update `tick_cache_by_id`/`_by_name`; append to `tick_history_by_id[id]` (oldest dropped beyond `tick_history_limit`); update the LRU order and,
if `max_tick_symbols > 0`, evict the least recently updated symbol's history. Read with `symbol_info_tick(name)`, `tick_history_stats()`,
`clear_tick_history(Option<id>)`, `copy_ticks_from/range`.

### `copy_ticks_*`

A **view over the history collected from pushes** (subscribe first), not a server query.
`copy_ticks_from(symbol, from, count, flags)`: ticks with `tick_time_ms >= from`, filtered by flags, first `count`.
`copy_ticks_range(symbol, from, to, flags)`: closed interval; an integer-second `to` includes the whole second (`.999`).
Flags: `COPY_TICKS_INFO = 1` (has bid/ask), `COPY_TICKS_TRADE = 2` (has last/volume), `COPY_TICKS_ALL = 3`.
Items: `{time, time_msc, bid, ask, last, volume, volume_real, flags, symbol}`.

## 4. Order book (DOM)

`subscribe_book`, `unsubscribe_book`, `subscribe_book_by_name` follow the same "send the full set" rule as ticks. The cmd 23 push is
`[u32 n]( BOOK_HEADER 22 B, BOOK_LEVEL 16 B x (bid_count + ask_count) ) x n` -> `{ symbol_id, symbol?, bids: [{price, volume}], asks: [...] }`.
Cache: `market_book_get(name)`; `market_book_add/release(name)` subscribe/unsubscribe by name. **Status: unit-tested only; the tested brokers did not serve DOM.**

## 5. Bars (cmd 11)

```rust
client.get_rates("EURUSDc", 60, from_unix_s, to_unix_s).await?   // period in minutes
```

Request: `symbol (64 B) | period u16 (protocol code) | from i32 | to i32`. Result: `time, open, high, low, close, tick_volume, spread` (+ `real_volume`).
Period codes: 1, 5, 15, 30 unchanged; 60 -> 16385, 240 -> 16388, 1440 -> 16408, 10080 -> 32769, 43200 -> 49153 (`period_map()`).

### Choosing the bar layout (fixed relative to pymt5)

Standard servers send 48-byte bars; some send 56-byte bars with `real_volume`. A body length divisible by both (multiples of 336) is ambiguous: pymt5 always
chose 56 and produced garbage on standard servers (seen live: an 18-bar answer that was really 21 bars). `parse_rate_bars` now picks the only layout that
fits; if both fit it tries 48 first and accepts it when every timestamp is within 2000-2100 and non-decreasing, otherwise tries 56.

### MetaTrader5-style wrappers

| method | behaviour |
|---|---|
| `copy_rates_range(sym, tf, from, to)` | plain `get_rates` |
| `copy_rates_from(sym, tf, to, count)` | fetches a lookback window and returns the last `count` bars |
| `copy_rates_from_pos(sym, tf, start_pos, count)` | up to "now", then cuts counting from the latest bar (`start_pos = 0` is the latest) |

`timeframe` accepts minutes (60) or the protocol code (16385). `history_lookback_seconds(pm, bars) = max(60, pm*60) * max(16, bars*4+8)`. The number of bars
returned depends on what the server has (closed markets can yield fewer than `count`).

## 6. Spreads and groups

* `get_symbol_groups()` - cmd 9, `[n][256 B UTF-16 name]*`.
* `get_spreads(&ids)` - cmd 20 (empty `ids` sends an empty payload). Untested live.
