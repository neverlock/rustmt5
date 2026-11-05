# Trading (`src/trading.rs`, `src/order_helpers.rs`, `src/currency.rs`)

> ⚠ **สถานะการทดสอบ:** การ *อ่าน* (positions, orders, history, `order_check`, `order_calc_*`) ยืนยันกับเซิร์ฟเวอร์จริงแล้ว
> แต่ **ไม่เคยส่งออเดอร์จริง** (`trade_request` ทดสอบกับ mock เท่านั้น). ทดลองบน demo ก่อนเสมอ

## 1. Positions / orders / history

| เมธอด | cmd | หมายเหตุ |
|---|---|---|
| `get_positions_and_orders()` | 4 | body = `[n_pos][POSITION 344 B]*[n_ord][ORDER 356 B]*` (`split_positions_orders`) |
| `get_positions()`, `get_orders()`, `positions_total()`, `orders_total()` | 4 | shortcut |
| `get_trade_history(from, to)` | 5 | `from/to` เป็น unix sec (i32) 0 = ไม่จำกัด; ผล `{deals, orders}` |
| `get_deals(from, to)` | 5 | |

* ใน deal: `time_create`/`time_update` ถูกแปลงเป็น **มิลลิวินาที** = `sec*1000 + *_ms`
  (ข้อมูลอื่นเช่น position ยังเป็นวินาที — ระวังหน่วยไม่เท่ากัน)
* ตำแหน่งที่ใช้เทียบ id: position → `position_id`; order/deal → `trade_order`
* `positions_get(symbol, group, ticket)` / `orders_get(...)`: **ตัวกรองแรกที่ให้มาเท่านั้นที่มีผล** (symbol > group > ticket) ตามต้นฉบับ
* `history_orders_get/history_deals_get(from, to, &HistoryFilter{group,ticket,position})` — กรองหลายเงื่อนไขพร้อมกันได้ (AND)

## 2. Volume encoding (สำคัญ)

เซิร์ฟเวอร์ใช้จำนวนเต็ม `lots × 10^8` (`VOLUME_PRECISION = 8`) ทั้งใน request และในฟิลด์ `volume_min/max/step/limit`

```rust
Mt5Client::volume_to_lots(0.01, 8) == 1_000_000      // round half-even
```

* helper ระดับสูง (`buy_market` …) และ `order_send` รับ **lots แบบ f64** แล้วแปลงให้
* `trade_request(&TradeRequest)` รับ **wire volume (u64)** ตรง ๆ
* ค่าความละเอียด 8 หลักเป็นสมมติฐานจากเซิร์ฟเวอร์ที่ทดสอบ (`volume_min = 1_000_000` สำหรับ 0.01 lot, ยืนยันจริง) —
  ถ้า broker ใดใช้ความละเอียดอื่น ต้องแก้ `VOLUME_PRECISION`

## 3. `trade_request` (cmd 12)

`TradeRequest` → `TRADE_REQUEST_SCHEMA` (236 B), ลำดับ field:
`action_id, trade_action, symbol(FS64), volume(u64), digits, order(u64), trade_type, type_filling, type_time, type_flags, type_reason,
price_order, price_trigger, price_sl, price_tp, deviation(u64), comment(FS64), position_id, position_by, time_expiration`

การตรวจก่อนส่ง (คืน `Error::Validation` ไม่ยิงเซิร์ฟเวอร์):

| trade_action | เงื่อนไข |
|---|---|
| `DEAL`(1), `PENDING`(5) | `volume > 0` |
| `PENDING` | `price_order > 0` |
| `MODIFY`(7), `REMOVE`(8) | `order > 0` |
| `SLTP`(6) | `position_id > 0` |
| `CLOSE_BY`(10) | `position_id > 0` และ `position_by > 0` |
| ทุกแบบ | `price_sl >= 0`, `price_tp >= 0` |

symbol/comment ถูกตัดที่ 32 ตัวอักษร (field กว้าง 64 B)

### ตอบกลับ → `TradeResult`

* body ว่าง → `retcode=-1, "Empty response", success=false`
* `retcode = u32(body[0..4])`; `description = retcode_text(retcode)`; `success = retcode ∈ {0, 10009 DONE, 10010 DONE_PARTIAL, 10008 PLACED}`
* ถ้า `body.len() >= 120` (`TRADE_RESPONSE_SCHEMA`) → เติม `deal, order, volume(wire), price, bid, ask, comment, request_id`
  (`volume` ใน `TradeResult` เป็น **wire volume** ไม่ได้แปลงกลับเป็น lot)

> `success=true` ไม่ได้แปลว่าออเดอร์ถูก fill เสมอ — `PLACED` (10008) = วาง pending สำเร็จ ตรวจ `retcode` ประกอบ

### Retcodes ที่ใช้บ่อย

`10009` done · `10008` placed · `10010` partial · `10004` requote · `10006` reject · `10013` invalid request · `10014` invalid volume ·
`10015` invalid price · `10016` invalid stops · `10017` trade disabled · `10018` market closed · `10019` no money · `10030` invalid fill type
(ตารางเต็มใน `constants::retcode_description`)

## 4. Order helpers

```rust
use rustmt5::{MarketOrderOptions, PendingOrderOptions, CloseOptions};

client.buy_market("EURUSDp", 0.01, &MarketOrderOptions { sl: 1.05, tp: 1.15, ..Default::default() }).await?;
client.sell_limit("EURUSDp", 0.01, 1.2, &PendingOrderOptions::default()).await?;
client.buy_stop_limit("EURUSDp", 0.01, /*stop trigger*/ 1.12, /*limit*/ 1.119, &Default::default()).await?;
client.close_position("EURUSDp", position_id, 0.01, &CloseOptions::default()).await?;
```

| helper | trade_action | trade_type | price field |
|---|---|---|---|
| `buy_market` / `sell_market` | DEAL | BUY / SELL | `price_order = 0` (เซิร์ฟเวอร์ใช้ราคาปัจจุบัน) |
| `buy_limit` / `sell_limit` | PENDING | BUY_LIMIT / SELL_LIMIT | `price_order = price` |
| `buy_stop` / `sell_stop` | PENDING | BUY_STOP / SELL_STOP | `price_order = price` |
| `buy_stop_limit` / `sell_stop_limit` | PENDING | BUY/SELL_STOP_LIMIT | **`price_trigger = price`**, `price_order = stop_limit_price` |
| `close_position` | DEAL | ตรงข้ามของ position | `position_id` |
| `close_position_by` | CLOSE_BY | – | `position_id`, `position_by` |
| `modify_position_sltp` | SLTP | – | `price_sl/tp` |
| `modify_pending_order` | MODIFY | – | `price_order`, `type_time`, `time_expiration` |
| `cancel_pending_order` | REMOVE | – | `order` |

* ค่าเริ่มต้น: `deviation = 20` (market/close), `filling = FOK(0)`, `time_type = GTC(0)`, `digits = None` (หาจาก cache, ไม่เจอใช้ 5)
* `magic` ถูกส่งใน field `type_reason`
* `close_position` ไม่ระบุ `order_type` → เรียก `get_positions()` หา position: `trade_action == SELL(1)` → ปิดด้วย BUY ไม่งั้น SELL;
  **หาไม่เจอจะ default เป็น SELL พร้อม warning** (อาจเปิด short ใหม่โดยไม่ตั้งใจ — ระบุ `order_type` เองถ้าไม่แน่ใจ)
* `place_order` ตรวจ `validate_symbol_name` และ `validate_volume` ก่อนส่ง

## 5. สูตร profit / margin (`currency.rs`)

คำนวณในเครื่อง (เซิร์ฟเวอร์ไม่มี RPC ให้) เลียนแบบสูตรของ Web Terminal. `trade_calc_mode` = ชนิดสูตรของ symbol

### Profit (`calc_profit_raw`) — คืนหน่วย "สกุลกำไรของ symbol" ก่อนแปลง

| calc_mode | สูตร | ต้องมี (ไม่งั้น `last_error`) |
|---|---|---|
| 37, 39 (bond) | `dir·((pc/100·face + accrued) − po/100·face)·contract·vol` | contract, face_value (-10) |
| 64 (collateral) | 0 | |
| 0, 5 (forex), 2, 3, 4, 32, 38 (CFD) และ mode อื่น | `dir·(pc − po)·contract·vol` | contract_size (-11) |
| 1, 33, 35, 36 (futures/options) | `dir·(pc − po)·vol·pv` | tick_value (-12) |
| 34 (FORTS) | `dir·(f(pc) − f(po))·vol`, `f(p)=p·tick_value/tick_size` | tick_value (-12) |

`dir = +1` ถ้า buy, `−1` ถ้า sell; `pv = tick_value/tick_size` (ถ้า `tick_size = 0` ใช้ `tick_value`)

### Margin (`calc_margin_raw`)

| calc_mode | สูตร | error |
|---|---|---|
| 37, 39 (bond) | `lots·contract·face·price/100` | -13 |
| 64 | 0 | |
| 5 | `lots·contract` | -14 |
| 0 (forex) | `lots·contract / leverage` | -14, -15 (leverage ≤ 0) |
| 4 (CFD leverage) | `lots·contract / leverage · price` | -14, -15 |
| 1, 33, 34 (futures) | `margin_initial>0 ? lots·margin_initial : lots·price·pv` | -16 |
| 3 (CFD index) | `lots·contract·price·pv` | -17 |
| 2, 32, 35, 36, 38 | `(mode∈{32,35,36,38} && margin_initial>0) ? lots·margin_initial : lots·contract·price` | -14 |
| อื่น ๆ | – | -18 |

### การแปลงสกุลเงิน (`resolve_conversion_rates`)

เป้าหมาย: แปลง "สกุลกำไร/มาร์จิ้น" → "สกุลบัญชี" (`RUB`≡`RUR`)
1. สกุลเท่ากัน → (1, 1)
2. หาคู่ตรง `source→target`: ชื่อ `"{src}{dst}"` → ไม่เจอลองชื่อที่ขึ้นต้น src ลงท้าย dst (เรียงตัวอักษร) →
   **ไม่เจออีกลองชื่อที่ขึ้นต้นด้วย `{src}{dst}` (รองรับ suffix ของ broker เช่น `EURUSDp`)**
3. ราคาจาก tick cache (`bid`, `ask`); ถ้าคู่นั้นคือ symbol ปัจจุบันและ cache ว่าง ใช้ราคา `fallback` (ราคาที่ผู้ใช้ให้)
4. ไม่เจอคู่ตรง ลองคู่กลับด้าน (ใช้ `1/bid` หรือ `1/ask`); ยังไม่ได้ → สามเหลี่ยมผ่าน USD
5. buy ใช้ ask, sell ใช้ bid สำหรับคู่ตรง (กลับด้านสลับกัน)

การใช้อัตรา: profit โหมด forex → `raw·(rate_buy ถ้า buy ไม่งั้น rate_sell)`; โหมดอื่น → `raw·(raw>0 ? rate_sell : rate_buy)`; margin → `raw·(buy ? rate_buy : rate_sell)`
**ต้อง subscribe tick ของคู่แปลงไว้ก่อน** (หรือเป็น symbol เดียวกับที่ถาม) ไม่งั้นแปลงไม่ได้ (`-5`/`-6`)

คืน `Result<Option<f64>>`: `Ok(None)` + `last_error()` เมื่อคำนวณไม่ได้ตามปกติ; `Err` เฉพาะข้อผิดพลาดการเชื่อมต่อ/เซสชัน

## 6. `order_send` (dict แบบ MetaTrader5)

`order_send(&Record)` รับคีย์: `action, symbol, volume(lots), type, price, stoplimit, sl, tp, deviation, comment, position, position_by, order,
type_filling (default FOK), type_time (default GTC), type_flags, type_reason, expiration, action_id|request_id`
กฎ: ถ้า `type` เป็น `*_STOP_LIMIT` → `price_trigger = price`, `price_order = stoplimit`; ไม่งั้น `price_order = price`. แล้วเรียก `trade_request`

## 7. `order_check` — pre-flight ในเครื่อง

> **ไม่ใช่ RPC ของเซิร์ฟเวอร์** — ตรวจกฎเท่าที่รู้จากข้อมูล symbol/บัญชี ผล "ผ่าน" ไม่ได้รับประกันว่าเซิร์ฟเวอร์จะรับ

ขั้นตอน (`order_check_inner`):

```
normalized = normalize_order_request(request)         // เติม default, คำนวณ price_order/price_trigger, expiration
(retcode, comment) = validate_order_check_request:
    action ∈ {DEAL, PENDING}: validate_deal_or_pending
    SLTP/MODIFY/REMOVE/CLOSE_BY: ตรวจว่ามี position/order ที่ต้องใช้
    อื่น ๆ: 10013 invalid
account = get_account()
ถ้า retcode ∈ {0, 10008, 10009, 10010}:  margin เพิ่ม = estimate_order_margin(...)   // Option
margin_post = account.margin + additional ; margin_free = equity − margin_post
margin_level = equity / margin_post · 100  (0 ถ้า margin_post ≤ 0)
ถ้ายังผ่านแต่ margin_free < 0 → retcode = 10019 "Not enough money"
```

`validate_deal_or_pending` ตรวจตามลำดับ (คืน retcode แรกที่ผิด):

1. symbol ว่าง → 10013 · ไม่พบ symbol → 10013 `symbol not found`
2. `volume <= 0` → 10014
3. `type` ไม่ใช่ order type ที่รู้จัก → 10035
4. `trade_mode`: 0 → 10017 disabled · 1 (long only) แต่เป็น sell → 10035 · 2 (short only) แต่เป็น buy → 10035 · 3 (close only) → 10017
5. **volume เทียบ min/max/step** → 10014 (ข้อความ `below symbol minimum` / `above symbol maximum` / `does not align with step`)
   — เทียบหลัง **แปลง wire → lots** แล้ว (`symbol_info_volumes_in_lots`)
6. `type_time` เป็น SPECIFIED/SPECIFIED_DAY แต่ไม่มี `expiration` → 10022
7. `type_filling` ไม่อยู่ใน {0,1,2} (เมื่อ symbol มี fill flags) → 10030
8. หาราคาอ้างอิงไม่ได้ (`price`, pending → `price_order`/`price_trigger`, market → tick cache) → 10015
9. ระยะ SL/TP/trigger ใกล้กว่า `point·trade_stops_level` → 10016

คีย์ของผลลัพธ์: `retcode, balance, equity, profit, margin, margin_free, margin_level, comment, margin_known, request`

* `margin_known = false` เมื่อประเมินมาร์จิ้นของออเดอร์ไม่ได้ (เช่นไม่มีอัตราแปลงสกุลเงิน/ไม่มีราคา) — ผลที่ "ผ่าน" แล้วจึงไม่แน่นอน
* ถ้า parse ผิดพลาดกลางทาง (`Protocol`/`Validation`) → ตอบ `retcode = -99` พร้อมค่าบัญชีและข้อความ

### ผลทดสอบจริง (EURUSDp, build 6182)

| volume (lot) | retcode | comment |
|---|---|---|
| 0.01 | 0 | OK |
| 0.001 | 10014 | volume 0.001 is below symbol minimum 0.01 |
| 0.015 | 10014 | volume 0.015 does not align with step 0.01 |
| 5000 | 10014 | volume 5000 is above symbol maximum 1000 |

### ข้อจำกัดที่ทราบ

* **ไม่รู้ margin เดิมของบัญชี:** response cmd 3 (`ACCOUNT_WEB_MAIN`) **ไม่มี field `margin`/`margin_free`/`margin_level`**
  ดังนั้น `account.margin` = 0 เสมอ และ `equity = balance + credit + acc_profit` — `order_check` จึงไม่นับมาร์จิ้นของตำแหน่งที่เปิดอยู่
  (ค่าจริงมากับ push cmd 10 แบบ balance update: `TRADE_UPDATE_BALANCE`) ถ้าบัญชีมีตำแหน่งเปิดอยู่ ผล `margin_free` จะสูงเกินจริง
* ไม่ตรวจ freeze level, ข้อจำกัดจำนวน pending, เวลาเทรด (trade sessions), หรือ hedging mode
* `tick_size` ของ broker ที่ทดสอบเป็น 0 — สูตร futures/FORTS ที่ต้องใช้ tick_size จะใช้ `tick_value` ตรง ๆ (อาจไม่ตรง broker อื่น)
