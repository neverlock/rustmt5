# Protocol

Reverse-engineered from the MT5 Web Terminal JavaScript bundle (by pymt5) and checked against a live build 6182 server.
Binary over WebSocket, everything **little endian**.

## 1. Connection

* URL, e.g. `wss://web.metatrader.app/terminal` (white-label brokers use their own host/path, e.g. `wss://host/terminala`).
* Required header: `Origin: https://web.metatrader.app`.
* No message size limit (symbol lists and history are large). **No WebSocket ping** - cmd 51 is the application heartbeat.
* Only binary messages are used; text messages are ignored.

## 2. Outer frame

```
 0        4        8
 +--------+--------+----------------------------
 | len u32| ver u32| body (len bytes = ciphertext)
 +--------+--------+----------------------------
```
`ver` is `OUTER_PROTOCOL_VERSION` (1). `unpack_outer` checks `len == body.len()` (`Error::Protocol("frame length mismatch ...")`).

## 3. Inner frame (after decryption)

Request (client -> server): `rand(2) | cmd u16 | payload`.
Response/push (server -> client): `2 bytes | cmd u16 | code u8 | body` (at least 5 bytes).

`code == 0` is success. A non-zero code is a command-level error and the body is usually empty
(login with bad credentials -> `code=3`; cmd 18 with the old name-based request -> `code=1`).

## 4. Encryption

AES-CBC, **IV = 16 zero bytes**, PKCS7 padding. Before the handshake the *initial key* (32 bytes) is used: it is stored obfuscated in
`INITIAL_KEY_OBFUSCATED` (decode: subtract 1 from each char; code 28 -> `&`, 23 -> `!`; then hex -> bytes). After bootstrap the
*session key* sent by the server (16/24/32 bytes; 32 observed) is used.

## 5. Handshake (cmd 0)

```
Client                                         Server
  | cmd 0, payload = 64 zero bytes (token) -->  |   (initial key)
  | <-- code 0, body = build u16 | token 64 | session_key   (initial key)
  | switch to the session key; remember token; server_build = build
  | (optional) cmd 29 init_session
  | cmd 28 login ...
```

Sizes verified against a browser capture (regression tests): bootstrap 68 B inner -> 80 B encrypted -> 88 B framed; login payload
912 B -> 916 -> 928 -> 936; init payload 744 B -> 748 -> 752 -> 760.

## 6. Primitive types (`PropType`)

| PropType | id | wire | parsed `Value` | notes |
|---|---|---|---|---|
| I8/I16/I32/I64 | 1/2/3/17 | 1/2/4/8 B signed | `Int` | |
| U8/U16/U32/U64 | 4/5/6/18 | 1/2/4/8 B unsigned | `UInt` | |
| F32/F64 | 7/8 | IEEE-754 | `Float` | |
| Time | 9 | 8 B **Windows FILETIME** | `Int` (unix ms) | `ft = (unix_ms + 11_644_473_600_000) * 10_000` |
| String | 10 | N B latin-1, NUL padded | `Str` | rarely used |
| FixedString | 11 | N B **UTF-16LE**, NUL padded | `Str` | N is in **bytes** (N/2 characters); stops at the first NUL |
| Bytes | 12 | N B | `Bytes` | |

Serialisation rules (`SeriesCodec::serialize`): out-of-range integers are an error (`U32 value out of range`), never wrapped;
over-long strings are truncated to N bytes; `SerField::raw` appends bytes without padding; clients first truncate by *characters*
(`trunc(s, 32)`) and then encode into the wider field. `SeriesCodec::parse` first checks that `buf.len() - offset >= get_series_size(schema)`.

## 7. Common body shapes

| Shape | Used by | Parser |
|---|---|---|
| `[u32 n][record]*` | symbols, positions, orders, spreads, corporate links, full symbols | `parsers::parse_counted_records` |
| single record | account, login, trade ack | `SeriesCodec::parse` |
| `[u32 n][u32 id]*` | requests: subscribe ticks/book, spreads, **full symbols (cmd 18)** | `id_list_payload` |
| two counted blocks | cmd 4 (positions then orders), cmd 5 (deals then orders) | `split_positions_orders`, `get_trade_history` |
| `len / record_size` records, no count | tick push, bars, symbol details | `parse_tick_batch`, `parse_rate_bars` |
| `[4 B][zlib]` | compressed symbols (cmd 34) | `get_symbols` |

List parsers stop silently when the body is shorter than the count claims. If data seems to be missing, compare `body.len()` with the
schema size first - several layouts differ between server builds (see sections 9-11).

## 8. Login payload (cmd 28) - 912 bytes

| offset | size | field |
|---|---|---|
| 0 | 4 | version u32 (0 works) |
| 4 | 64 | password, UTF-16LE (32 chars; empty if a 320-hex password blob is used) |
| 68 | 128 | otp (64 chars) |
| 196 | 16 | client id: first 16 bytes of SHA-1 over `platform;dpr;lang;screen;uniq` (or the given `cid`) |
| 212 | 64 | utm_campaign |
| 276 | 64 | utm_source |
| 340 | 8 | lead_cookie_id u64 |
| 348 | 128 | lead_affiliate_site |
| 476 | 4 | len(url) u32 (max 128) |
| 480 | 256 | url |
| 736 | 8 | login u64 |
| 744 | 160 | password blob (zeros for normal passwords) |
| 904 | 8 | session u64 |

A password of exactly **320 characters** is interpreted as the hex of a 160-byte blob. The response is `[160 B token][u64 session]`; with
`code != 0` the client returns `Error::Authentication`.

> **Gotcha:** a `#` in an environment variable or `.env` file is often treated as the start of a comment, silently shortening the
> password -> `code=3`. This was the real cause of a failed login during development. Compare password *lengths* before suspecting the protocol.

## 9. Trade request (cmd 12) - 248 bytes

pymt5's `TRADE_REQUEST_SCHEMA` (236 B: deviation u64, no top/bottom) is **wrong**. The Web Terminal (`getTradeRequest`) sends the
layout in [reference/schemas.md](reference/schemas.md#corrections-layouts-that-differ-from-the-pymt5-schemas-above): `price_deviation` is a
`u32`, `price_top`/`price_bottom` (f64) precede `comment`. With pymt5's layout every field after `price_tp` is shifted, so the server
ignores e.g. `position_id` - a "close" then **opens an opposite position**.

The response of cmd 12 is only a **4-byte ack** (`u32 status`, 0 = accepted). Servers that answer with a full 120-byte result (older
layout) are also handled.

## 10. Trade result push (cmd 19) - 380 bytes

`u32 token | request echo (248 B) | result (128 B)`. Result fields: `retcode u32, deal i64, order i64, volume i64, price f64,
res_time u32, bid f64, ask f64, last f64, comment (64 B)`. The older pymt5 shape (`TRADE_RESULT_PUSH` 244 B + `TRADE_RESULT_RESPONSE`
108 B = 352 B) is used when the body is shorter than 380.

One request produces **several** pushes with the same `action_id`: `10002` (accepted) ... then a final code (`10009` done, `10008`
placed, or a rejection such as `10030`, `10013`, `10015`). Codes 10001-10003 are intermediate. The echoed `action_id` is how a request is
matched to its result - the client assigns a unique non-zero id.

## 11. Trade update push (cmd 10)

See the table in [reference/schemas.md](reference/schemas.md#corrections-layouts-that-differ-from-the-pymt5-schemas-above):

* `update_type 2` (760 B): `type, transaction_id, reserved` + DEAL (356) + POSITION (344) + 6 f64. A closed position has `trade_volume = 0`.
* `update_type 0/1` (368 B, 736 B for some deletes): `type, transaction_id, transaction_type` + ORDER (356).
  `0` = open-orders change (transaction 0 add / 1 update / 2 delete), `1` = a history order was added (the order is final).

The six f64 "balance" values do **not** contain used margin (see [trading.md](trading.md)).

## 12. cmd 4 (positions and orders)

`[u32 n_pos][POSITION(344) x n][u32 n_ord][ORDER(356) x n]` plus a 12-byte trailer on build 6182. **The answer is a snapshot taken at
login time** and is not refreshed during the session; changes arrive through cmd 10. (Orders appeared to update, positions did not; do not
rely on cmd 4 after login.)

## 13. Prices in ticks

Depending on the server, tick prices are real numbers (live CXMDirect: `80709.1`) or integers scaled by `10^digits` (demo CXMDirect:
`112623` for 1.12623). `normalize_tick_price_value` undoes the scaling heuristically; `ClientConfig::normalize_ticks = false` disables it
(see [market-data.md](market-data.md)).

## 14. Schemas and commands

Field tables: [reference/schemas.md](reference/schemas.md) (generated). Commands: [reference/commands.md](reference/commands.md).
Do not edit `src/schemas.rs` by hand; regenerate it with `tools/gen_schemas.py` (reads the pymt5 sources with `ast`, without executing them).
