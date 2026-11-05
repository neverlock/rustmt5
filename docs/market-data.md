# Market data (`src/market_data.rs`, `src/parsers.rs`, `src/push_handlers.rs`)

## 1. Symbols

### `load_symbols(use_gzip)`

```
get_symbols(true)  → cmd 34 → body = [4 B header][zlib stream]
                     body.len() <= 4  → []                      (ไม่มีข้อมูล)
                     zlib decompress (spawn_blocking) → [u32 n][SYMBOL_BASIC 526 B]*
get_symbols(false) → cmd 6  → [u32 n][SYMBOL_BASIC]*
```

จากนั้นล้าง `symbols`, `symbols_by_id`, `full_symbols` แล้วสร้าง `SymbolInfo` (name, id, digits, description, path, trade_calc_mode, basis, sector)
`symbol_cache_ttl > 0` และ cache ยังไม่หมดอายุ → คืน cache เดิมโดยไม่ยิงเซิร์ฟเวอร์ (`invalidate_symbol_cache()` บังคับโหลดใหม่)

ยืนยันจริง: 968 symbols, 13 กลุ่ม (ชื่อกลุ่มใช้ `\` เป็นตัวคั่น เช่น `C3\FX MajorsP`)

> Broker บางรายใส่ suffix ท้ายชื่อ (`EURUSDp`, `EURUSD.m`) — ใช้ชื่อตามที่เซิร์ฟเวอร์ส่ง (`symbol_names()`) ไม่เดาเอง

### Lookup helpers

| เมธอด | ทำอะไร |
|---|---|
| `get_symbol_info(name)` / `get_symbol_id(name)` / `symbol_names()` | อ่าน cache (ไม่ยิงเซิร์ฟเวอร์) |
| `resolve_symbol_id(name)` (internal) | cache → ถ้าไม่เจอ `load_symbols` แล้วหาใหม่ |
| `symbols_get(group, gzip)` | ดึงใหม่ทุกครั้ง + กรองด้วย glob mask |
| `symbols_total` | ขนาด cache ถ้ามี ไม่งั้นดึงมานับ |

### Group mask (`parsers::matches_group_mask`)

คั่นด้วย `,`; รายการขึ้นต้น `!` = ยกเว้น; ใช้ glob แบบ Python `fnmatchcase` (`*`, `?`, `[abc]`, `[a-z]`, `[!x]`, **case-sensitive**)
ตัวอย่าง `"EUR*,GBP*,!*JPY"`. mask ว่าง = ทุกตัว; ถ้ามีแต่รายการยกเว้น = ทุกตัวยกเว้นที่ระบุ

## 2. Full symbol info (cmd 18) — สเปคสัญญา

```rust
client.get_full_symbol_info("EURUSDp").await?   // Option<Record>
client.get_full_symbols(&[id1, id2]).await?     // RecordList (batch)
```

* **Request = `[u32 count][u32 symbol_id]*`** (ไม่ใช่ชื่อ symbol). pymt5 ส่งชื่อซึ่งเซิร์ฟเวอร์ build 6182 ปฏิเสธด้วย `code=1`;
  `get_full_symbol_info` จึงแปลงชื่อ→id ก่อน และมี fallback ส่งชื่อแบบเก่าสำหรับเซิร์ฟเวอร์รุ่นเก่า
* Response = `[u32 count][FULL_SYMBOL 3068 B]*`. สามฟิลด์ท้ายเป็น binary ซ้อน:
  * `trade` (628 B) = `ACCOUNT_WEB_TRADE_SETTINGS_SCHEMA` → ถูก parse เป็น Record ย่อย
  * `schedule` (896 B) = 7 วัน × 16 session × (u16,u16) สำหรับ quote แล้วต่อด้วย trade →
    `{ quote_sessions: [[[from,to]…]×7], trade_sessions: [...] }`
  * `subscription` (8 B) = `{ delay u32, status u8, level u8, reserved u16 }`
* `normalize_full_symbol_record` **ยกฟิลด์จาก `trade` ขึ้นมาไว้ระดับบน** เพื่อให้ใช้แบบ MetaTrader5:

  | คีย์ระดับบน | มาจาก `trade.` |
  |---|---|
  | `symbol_path`, `trade_mode`, `trade_stops_level`, `trade_freeze_level` | ชื่อเดียวกัน |
  | `volume_min/max/step/limit`, `margin_initial`, `margin_maintenance` | ชื่อเดียวกัน |
  | `filling_mode` / `expiration_mode` / `order_mode` | `trade_fill_flags` / `trade_time_flags` / `trade_order_flags` |
  | `trade_face_value`, `trade_accrued_interest` | `face_value`, `accrued_interest` (ระดับบนของ record) |

* ⚠ **`volume_min/max/step/limit` เป็น wire volume** (0.01 lot = 1 000 000). แปลงเป็น lot ด้วย
  `parsers::symbol_info_volumes_in_lots` ก่อนเทียบกับปริมาณของผู้ใช้ (`order_check` ทำให้แล้ว)
* `point` อยู่ระดับบนของ record; `tick_size` พบว่า **เป็น 0** ในข้อมูลจริงของ broker ที่ทดสอบ (สูตร profit/margin มีทางออกเมื่อ `tick_size = 0`)
* ผลถูก cache ใน `State.full_symbols` (key = ชื่อที่ถาม) จนกว่า `load_symbols` จะโหลดใหม่
* ถ้าเซิร์ฟเวอร์ตอบ `code ≠ 0` หรือ body ว่าง → `None` (ไม่ error)

`symbol_info(name)` = cache → cmd 18 → (ถ้ายังไม่มี) ข้อมูลพื้นฐานจาก `SymbolInfo` (มีแค่ id/digits/path/…)

## 3. Ticks

### Subscribe

```rust
client.subscribe_ticks(&[id]).await?;                 // รวมกับชุดเดิมแล้วส่งชุดเต็ม (sorted, dedup)
client.unsubscribe_ticks(&[id]).await?;               // ส่งชุดที่เหลือ; ว่าง = ส่ง [0] หยุดทั้งหมด
client.subscribe_symbols(&["EURUSDp"]).await?;        // ชื่อ → id; ไม่พบ = Error::SymbolNotFound
client.subscribe_symbols_batch(&[...]).await?;        // ข้ามชื่อที่ไม่พบเงียบ ๆ
let mut h = client.subscribe_ticks_managed(&ids).await?;  h.unsubscribe().await?;
```

เหตุที่ส่ง "ชุดเต็ม": cmd 7 **แทนที่** subscription ฝั่งเซิร์ฟเวอร์ทั้งชุด
`State.subscribed_ids` เก็บชุดล่าสุดไว้ใช้ซ้ำตอน reconnect

### Tick push → `Record`

`TICK_SCHEMA` (50 B): `symbol_id, tick_time(sec), fields, bid, ask, last, tick_volume, time_ms_delta, flags`
แล้วเติม `tick_time_ms = tick_time*1000 + time_ms_delta` และ `symbol` (ถ้ารู้ id)

**Partial update:** เซิร์ฟเวอร์ส่งเฉพาะราคาที่เปลี่ยน ส่วนที่ไม่เปลี่ยนเป็น `0.0`.
`parse_tick_batch(body, symbols_by_id, cache)` เติมค่า `bid/ask/last` ที่เป็น 0 จาก tick ล่าสุดใน cache ของ symbol เดียวกัน

> ข้อควรจำ: การ merge ใช้ cache *ที่มีอยู่ก่อน batch นี้* — ภายใน batch เดียวกันจะไม่เติมข้ามกัน (เหมือน pymt5).
> cache listener ถูกเรียกก่อน handler ผู้ใช้ จึงเห็นค่าที่ merge แล้ว

### การ normalise ราคา (`normalize_tick_price_value`) — ⚠ heuristic

pymt5 สังเกตว่าบางเซิร์ฟเวอร์ส่งราคาเป็น "จำนวนเต็มคูณ 10^digits" (เช่น `115364` แทน `1.15364`) จึงมีการคาดเดาแปลงกลับ:

```
digits == 0 หรือ price == 0            → ไม่แตะ
forex-like = ชื่อ 6 ตัวแรกเป็นตัวอักษรล้วน (เช่น EURUSD)
threshold  = 10^digits ; ถ้า digits>=4 && forex-like && ไม่มี JPY → 10^digits / 10
forex-like && digits>=3  → |price| >= threshold ? price/10^digits : price
อื่น ๆ                   → ถ้าไม่ใช่จำนวนเต็ม (|p - round(p)| > 1e-9) → ไม่แตะ
                            ถ้า |round(p)| < threshold → ไม่แตะ ; ไม่งั้น round(p)/10^digits
```

**ความเสี่ยงที่ทราบ:** กฎสุดท้ายถือว่า "ราคาที่เป็นจำนวนเต็มและ ≥ 10^digits" คือค่าที่ถูกคูณ
เช่น หุ้นราคา `200.00` (digits=2) จะกลายเป็น `2.0`, ทองคำ digits=2 ราคา `2000.0` → `20.0` (มี unit test ยืนยันพฤติกรรมนี้เพื่อให้ตรง pymt5)
ในการทดสอบจริงกับ build 6182 ราคาที่เซิร์ฟเวอร์ส่งเป็นราคาจริงอยู่แล้ว (เช่น `80709.1`) — ถ้าจะใช้กับหุ้น/ดัชนีให้ตรวจผลกับข้อมูลจริงก่อน
และพิจารณาเพิ่ม option ปิด normalisation (ยังไม่มี; ดู [contributing.md](contributing.md))

### Cache และ history (ติดตั้งใน `Mt5Client::new` → `install_cache_listeners`)

ทุก tick ที่ parse แล้ว:
1. `tick_cache_by_id[id] = tick` และ `tick_cache_by_name[name] = tick` (ถ้ารู้ชื่อ)
2. ต่อท้าย `tick_history_by_id[id]` (ตัวเก่าสุดหลุดเมื่อเกิน `tick_history_limit`)
3. ปรับ LRU; ถ้า `max_tick_symbols > 0` และมี symbol เกิน → ลบ history ของ symbol ที่ update นานสุด

อ่านด้วย `symbol_info_tick(name)`, `tick_history_stats()`, `clear_tick_history(Option<id>)`, `copy_ticks_from/range`

### `copy_ticks_*` (MetaTrader5-style)

ไม่ใช่การดึงจากเซิร์ฟเวอร์ — เป็น **มุมมองของ history ที่สะสมจาก push** เท่านั้น (ต้อง subscribe ไว้ก่อน)
* `copy_ticks_from(symbol, from, count, flags)` — tick ที่ `tick_time_ms >= from` กรองด้วย flags แล้วเอา `count` ตัวแรก
* `copy_ticks_range(symbol, from, to, flags)` — ช่วงปิด `[from, to]`; ถ้า `to` เป็นวินาทีจำนวนเต็มจะนับถึง `.999` ของวินาทีนั้น
* flags: `COPY_TICKS_INFO=1` (มี bid/ask), `COPY_TICKS_TRADE=2` (มี last/volume), `COPY_TICKS_ALL=3`
* ผลลัพธ์รูปแบบ `{time, time_msc, bid, ask, last, volume, volume_real, flags, symbol}`

## 4. Order book (DOM)

`subscribe_book(&ids)` / `unsubscribe_book` / `subscribe_book_by_name` (ใช้หลักเดียวกับ ticks: ส่งชุดเต็ม)
push cmd 23 = `[u32 n]( BOOK_HEADER(22 B) BOOK_LEVEL(16 B)×(bid_count+ask_count) )×n`
→ `{ symbol_id, symbol?, bids: [{price, volume}], asks: [...] }` (bids มาก่อน asks ตาม `bid_count`)
cache: `market_book_get(name)`; `market_book_add/release(name)` = subscribe/unsubscribe ตามชื่อ
(สถานะ: ทดสอบด้วย unit test เท่านั้น — เซิร์ฟเวอร์จริงที่ทดสอบยังไม่ได้เปิด DOM)

## 5. Bars (cmd 11)

```rust
client.get_rates("EURUSDp", 60, from_unix_sec, to_unix_sec).await?   // period เป็นนาที
```

Request = `symbol(FS64) ‖ period(u16 รหัส) ‖ from(i32) ‖ to(i32)`. ผล: `time, open, high, low, close, tick_volume, spread` (+ `real_volume` ถ้ามี)

### การเลือก layout ของ bar (แก้จาก pymt5)

เซิร์ฟเวอร์มาตรฐานส่ง bar 48 B; บางเซิร์ฟเวอร์ส่ง 56 B (มี `real_volume`). ความยาว body ที่หารได้ทั้ง 48 และ 56 (ผลคูณของ 336) กำกวม —
pymt5 เลือก 56 เสมอ ทำให้ได้ค่าขยะกับเซิร์ฟเวอร์มาตรฐาน (พบจริงกับ symbol ที่ได้ 18 bar → ควรเป็น 21)
`parse_rate_bars` จึง: หารลงตัวแบบเดียว → ใช้แบบนั้น; กำกวม → ลอง 48 ก่อน ถ้า timestamp ทุกตัวอยู่ใน 2000–2100 และไม่ลดลง ก็ใช้; ไม่งั้นลอง 56

### Wrappers แบบ MetaTrader5

| เมธอด | พฤติกรรม |
|---|---|
| `copy_rates_range(sym, tf, from, to)` | `get_rates` ตรง ๆ |
| `copy_rates_from(sym, tf, to, count)` | ดึงย้อนหลัง `history_lookback_seconds` แล้วเอา `count` แท่งท้าย |
| `copy_rates_from_pos(sym, tf, start_pos, count)` | ดึงถึง "ตอนนี้" แล้วตัดแบบนับจากแท่งล่าสุด (`start_pos=0` = ล่าสุด) |

`timeframe` รับได้ทั้งนาที (60) หรือรหัสโปรโตคอล (16385) — `normalize_timeframe_minutes`
`history_lookback_seconds(pm, bars) = max(60, pm·60) · max(16, bars·4+8)` (กว้างพอข้ามวันหยุด)
ข้อจำกัด: จำนวนแท่งที่ได้ขึ้นกับเซิร์ฟเวอร์/ช่วงเวลา; ถ้าตลาดปิดนานอาจได้น้อยกว่า `count`

## 6. Spreads / groups

* `get_symbol_groups()` — cmd 9, `[n][256 B UTF-16 name]*`
* `get_spreads(&ids)` — cmd 20 (ids ว่างส่ง payload ว่าง)
