# Account & Push notifications

## 1. Account response (cmd 3 / push cmd 14) — `parsers::parse_account_response`

Body = ต่อเนื่องกันดังนี้ (offset ไล่ไปเรื่อย ๆ):

| ส่วน | ขนาด | หมายเหตุ |
|---|---|---|
| `ACCOUNT_WEB_MAIN` | 816 B | ฟิลด์หลัก (ดู [schemas](reference/schemas.md)) — ต้องมีอย่างน้อยเท่านี้ ไม่งั้นคืน `Record` ว่าง |
| `u32 n` + `ACCOUNT_WEB_TRADE_SETTINGS`×n (628 B) | 4 + 628n | กฎราคา/volume/swap ต่อ path ของ symbol |
| `i32 trade_flags` | 4 | |
| `i32 symbols_count` | 4 | |
| `u64 leverage_flags` | 8 | |
| `i32 m` + (`LEVERAGE_RULE` 300 B + `tier_count`×`LEVERAGE_TIER` 32 B)×m | | `leverage_rules` (และ alias `rules`) |
| `u32 k` + (`COMMISSION` 304 B + `u32 t` + t×`COMMISSION_TIER` 80 B)×k | | `commissions` |

> ⚠ **Quirk ที่คงจาก pymt5:** ถ้า `n == 0` ตัวนับ 4 ไบต์ **ไม่ถูกข้าม** (offset ไม่ขยับ) ทำให้ `trade_flags`/`symbols_count` ที่ตามมาเพี้ยน
> เซิร์ฟเวอร์จริงส่ง `n ≥ 1` เสมอจึงไม่เจอ แต่ถ้าเขียน mock ให้ใส่ trade settings อย่างน้อย 1 รายการ

parser ทุกชั้นหยุดเมื่อ body หมด (ไม่ error) ถ้าพังกลางทางทั้งก้อน → `Record` ว่าง (ผู้เรียกควรเช็ก `is_empty()`)

### Field ที่คำนวณเพิ่ม

| คีย์ | ที่มา |
|---|---|
| `currency` | `account_currency` |
| `leverage` | `margin_leverage` |
| `name`, `server` | `account_name`, `server_name` |
| `daylight_mode`, `server_offset_time` | `daylightmode`; `timezone_shift + daylight_mode·3600` |
| `is_real/is_demo/is_contest/is_preliminary` | `account_type` = 0 / 1 / 2 / 3 |
| `trade_allowed` | `!(rights & 4)`; `is_trade_disabled` = ตรงข้าม |
| `is_investor` | `rights & 8` · `is_read_only` `rights & 512` · `is_reset_pass` `rights & 1024` |
| `is_hedged_margin` | `margin_mode == 2` |
| `risk_warning` | `permissions_flags & 16` |
| `profit` | `acc_profit` |
| `equity` | `balance + credit + profit` |
| `auth_password_min` | ค่าที่ได้ ถ้า 0 → 8 |

**ไม่มีใน response:** `margin`, `margin_free`, `margin_level` (จะอ่านได้ 0 จาก `get`) — ดู [trading.md](trading.md)
ค่าเหล่านี้มากับ push cmd 10 (balance update)

ตัวอย่างจริง (build 6182): `server="CXMDirect-Live" currency=USD leverage=1:1000 real=true trade_allowed=true`

### API ที่ห่อ cmd 3

| เมธอด | ผล |
|---|---|
| `get_account` / `account_info` | `Record` ข้างบน |
| `terminal_info` | `Record` สไตล์ MetaTrader5 (`build, company, name, server, connected, trade_allowed, tradeapi_disabled, timezone_shift, server_offset_time` …; path ต่าง ๆ ว่าง) |
| `version` | `Some((500, build, release_date))`; `release_date` จากตารางที่สังเกต (5687 → "15 Mar 2026"), build ที่ไม่รู้ = `""` |
| `get_account_summary` | `AccountInfo` + จำนวน positions/orders; ถ้า cmd 3 พังจะคำนวณ profit จาก positions |

## 2. คำสั่งบัญชีอื่น ๆ (ยังไม่เคยทดสอบกับเซิร์ฟเวอร์จริง)

| เมธอด | cmd | หมายเหตุ |
|---|---|---|
| `change_password(new, old, is_investor)` | 24 | คืน `i32` status (0 = สำเร็จ ตามที่เข้าใจ) — **เปลี่ยนรหัสจริง** |
| `trader_params` | 41 | `(String, String)` |
| `send_notification(msg)` | 42 | ตัดที่ 128 ตัวอักษร |
| `get_corporate_links` | 44 | `link_type, url, label, flags, icon_data` |
| `verify_code(code)` | 27 | |
| `open_demo_account(&DemoAccountRequest, cid, initialize)` | 30 | payload = `build_opening_base_payload` |
| `open_real_account(&RealAccountRequest, …)` | 39 | base + ฟิลด์เพิ่ม + 512 B reserved + เอกสาร (`AccountDocument`) |
| `request_opening_verification` / `submit_opening_verification` | 27 / 40 | คืน `VerificationStatus{email, phone}` |
| `enable_otp` / `disable_otp` | 43 | |

`build_opening_base_payload` เรียง 22 field (ชื่อเต็ม = `first second` ตัด 128, group, phone_password, country, city, state, zipcode,
address, phone, email, deposit f64, leverage u32, 0, 1, domain, utm_*, confirm codes, first/second name, agreements) — รวม 1 664 B
(มี unit test ตรวจขนาด). `birth_date_ms` ใช้ `PropType::Time` (FILETIME)

`initialize = true` จะส่ง `init_session` (cmd 29) ก่อน โดยใช้ `cid` เดียวกัน

## 3. Push handlers

ทุก `on_*` คืน `ListenerId` (ลบด้วย `client.transport().off(cmd, Some(id))`) และ callback เป็น closure `Fn(..) + Send + Sync + 'static` ที่รันบน recv task

| เมธอด | cmd | callback รับ | parse |
|---|---|---|---|
| `on_tick` | 8 | `&[Record]` (ทั้ง batch) | `parse_tick_batch` (+ merge cache + normalise) |
| `on_tick_event` | 8 | `&TickEvent` (ทีละ tick) | `timestamp` = `tick_time_ms` (ถ้า 0 ใช้ `tick_time`) |
| `on_position_update` / `on_order_update` | 4 | `&[Record]` | ส่วนที่เกี่ยวข้องของ cmd 4 |
| `on_trade_update` | 4 | `&TradeUpdate{positions, orders}` | cmd 4 ทั้งก้อน |
| `on_account_update` | 14 | `&Record` | `parse_account_response` (ว่างได้) |
| `on_account_event` | 14 | `&AccountEvent` | ข้ามถ้า parse ได้ว่าง; `margin/margin_free` มักเป็น 0 |
| `on_trade_result` | 19 | `&Record` | `TRADE_RESULT_PUSH` + `result: TRADE_RESULT_RESPONSE` |
| `on_trade_result_event` | 19 | `&TradeResultEvent` | `retcode/order/deal/volume/price/comment` จาก `result` |
| `on_trade_transaction` | 10 | `&Record` | ดูข้างล่าง |
| `on_book_update` / `on_book_event` | 23 | `&[Record]` / `&BookEvent` | `parse_book_entries` |
| `on_symbol_details` | 17 | `&[Record]` | `SYMBOL_DETAILS` ต่อกัน + `symbol` |
| `on_symbol_update` | 13 | `&CommandResult` | raw (schema ไม่แน่นอน) |
| `on_login_status` | 15 | `&CommandResult` | raw |
| `on_callback_error` | – | `&CallbackError{command, message}` | เรียกเมื่อ callback ใด panic |

### Trade transaction (cmd 10)

`update_type = u32(body[0..4])`

* `== 2` (balance update): `balance_info` (`TRADE_UPDATE_BALANCE`: balance, credit, acc_profit, equity, **margin, margin_free**) →
  `deals` (counted `DEAL`) → `positions` (counted `POSITION`; offset ข้าม deals ด้วย `4 + n·356`)
* อื่น ๆ: `TRADE_TRANSACTION` (`flag_mask`, `transaction_id`, `transaction_type` 0=add 1=update 2=delete) แล้วตามด้วย `order` (`ORDER`)

> ข้อสังเกต: listener ของ cmd 4 / 14 ถูกเรียกทั้งตอนมี **response** ของ `get_positions_and_orders()`/`get_account()` และตอนมี push จริง

### การเขียน handler ที่ปลอดภัย

```rust
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
client.on_tick_event(move |t| { let _ = tx.send(t.clone()); });   // ส่งออกทันที ไม่ทำงานหนักใน callback
tokio::spawn(async move { while let Some(t) = rx.recv().await { /* งานหนัก / async */ } });
```
