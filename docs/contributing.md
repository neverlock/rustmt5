# Contributing and maintenance

## 1. Workflow

```bash
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

The default branch is `master`. Never commit credentials; examples and tests read them from the environment and never print the password.

## 2. Recipes

### 2.1 Add a request/response command

1. If the id is missing from `VALID_COMMANDS` (`constants.rs`) add it (and a `CMD_*` constant; ids that come from pymt5 are regenerated into `constants_gen.rs`).
2. Add the schema: change pymt5's `schemas.py` and regenerate (2.4), or write a `const X_SCHEMA: &[Field]` in `parsers.rs` when pymt5's version is wrong (as was done for the trade request/result).
3. Add the method to the matching `impl Mt5Client` file:
   ```rust
   pub async fn get_foo(&self, id: u32) -> Result<RecordList> {
       let payload = SeriesCodec::serialize(&[SerField::u32(id)])?;
       let r = self.transport().send_command(CMD_GET_FOO, &payload).await?;
       if r.code != 0 { return Ok(vec![]); }          // some servers signal errors through `code` with an empty body
       Ok(parse_counted_records(&r.body, FOO_SCHEMA, FOO_FIELD_NAMES))
   }
   ```
4. Add a parser unit test, a mock arm and an integration assertion, and a row in [reference/commands.md](reference/commands.md).
5. If the layout is not from a verified capture, say so in the docs (the project's rule: document what is verified).

### 2.2 Add a push handler

1. Write a parser that never panics (use `get(..)`/`read_u32`, not indexing) in `parsers.rs`.
2. ```rust
   pub fn on_foo(&self, cb: impl Fn(&Record) + Send + Sync + 'static) -> ListenerId {
       self.transport().on(CMD_FOO_PUSH, move |r| cb(&parse_foo(&r.body)))
   }
   ```
3. If it needs client state, capture `self.inner.state.clone()` (not `self.clone()`), lock briefly inside the closure.
4. If the client itself must track it (like trade updates), add it to `install_cache_listeners` so it runs before user handlers.

### 2.3 Change or add record fields

Edit pymt5's `schemas.py` and regenerate, or add a hand-written override next to its parser (with a comment saying why and how it was verified).
`len(schema)` must equal `len(names)`; the schema size tests in `protocol.rs` catch layout drift.

### 2.4 Regenerate from pymt5

```bash
git clone https://github.com/cloudQuant/pymt5 /tmp/pymt5
python3 -I tools/gen_schemas.py      /tmp/pymt5 src/schemas.rs
python3 -I tools/gen_constants.py    /tmp/pymt5 src/constants_gen.rs
python3 -I tools/gen_schema_docs.py  /tmp/pymt5 docs/reference/schemas.md
```
The scripts read the Python with `ast` and **never execute** it - keep it that way (it is third-party code). They understand only literals, constant names and `list + list`.
The hand-written parts of `constants.rs` (`VALID_COMMANDS`, retcode table, calc-mode sets) must be updated manually if upstream changes.

### 2.5 Add a `ClientConfig` option

Add the field and `Default` in `client.rs`, use it in `Mt5Client::new`/`Transport::with_options`, add a row to the table in [client.md](client.md).

## 3. Conventions

* Server-facing functions return `Result<T>`. MetaTrader5-style "soft failing" helpers return `Result<Option<T>>` and call `fail_last_error`.
* **Never hold `self.st()` (a `std::sync::MutexGuard`) across `.await`.** Inside a `tokio::spawn`ed future the compiler rejects it; elsewhere it compiles and can deadlock, so scope the guard with `{ ... }`.
* Truncate text by characters with `trunc` before encoding into byte-sized fields.
* Protocol enum constants are `i64` (to compare with `Record` values); serialised integers use `SerField::u32(..)` or `SerField::new(PropType::U32, i64, None)` (range-checked).
* Log with `tracing`; never log passwords or tokens.
* **When pymt5 and the server disagree, trust the server and the Web Terminal JS** (`analysis/*.js` in the pymt5 repo), and add a test that pins the observed layout.

## 4. Differences from pymt5

| Topic | pymt5 | rustmt5 | Why |
|---|---|---|---|
| reconnect | new transport, listeners lost | same transport | keep callbacks/caches |
| login result | not checked | `Error::Authentication` | readable error |
| server closes the socket normally | not a disconnect | disconnect | auto-reconnect works |
| cmd 18 request | symbol name | symbol id (+ name fallback) | build 6182 rejects names |
| bars layout | always 56 B when divisible | plausibility check, 48 B first | garbage data seen live |
| **cmd 12 request layout** | 236 B | **248 B** (JS layout) | `position_id` etc. were ignored; "close" opened a new position |
| **trade result** | cmd 12 answer taken as the result | waits for the final cmd 19 push (`action_id`) | rejected orders were reported as success |
| cmd 19 / cmd 10 / cmd 4 layouts | pymt5 layouts | build 6182 layouts, pymt5 as fallback | observed on the server |
| positions/orders | cmd 4 each time | live state from pushes | cmd 4 is a login-time snapshot |
| **order helpers** | `trade_action = 1`, FOK | action/filling by execution mode, `type_flags`, lookups | what the Web Terminal sends and the server accepts |
| `order_send` | thin wrapper | dispatches dict -> helpers | works with the above |
| used margin | read from the account (always 0) | computed locally | the server never sends it |
| conversion symbol | start/end match | + `startswith(base+quote)`; cent currencies | broker suffixes, USC accounts |
| `order_check` | wire volumes vs lots; silent margin 0 | converted; `margin_known` flag | wrong results |
| tick normalisation | always | `normalize_ticks` option | live server sends real prices |
| callbacks | sync/async | sync closures | language |
| pandas, `PYMT5_*` env | yes | no | n/a |

## 5. pymt5 quirks kept on purpose

* an empty trade-settings count is not skipped in the account response;
* the tick price heuristic (see [market-data.md](market-data.md));
* tick merging uses the cache from before the batch;
* `positions_get`/`orders_get` apply only the first filter given;
* a late reply after a timeout is matched to the next waiter (no request ids);
* deals use milliseconds, positions/orders seconds.

## 6. Open work

1. **Verify on a standard USD demo account** (new env vars were provided but were not visible in the session that wrote this): non-cent currency conversion, stop-limit, partial close, hedged
   close-by, SL/TP change pushes, `account_margin` against the broker's own margin numbers, other symbol classes (stocks/indices/crypto, instant or exchange execution).
2. Hedged-margin offsets in `account_margin`.
3. Re-price positions continuously (profit/`price_close` are as of the last push).
4. A "gave up" callback after the last reconnect attempt (`on_reconnect_failed`).
5. HTTP CONNECT proxy support for the WebSocket.
6. Detect the volume precision per server instead of the fixed `10^8`.
7. `Error::Trade` is unused (trade failures are `TradeResult`); consider an error-on-failure wrapper.
