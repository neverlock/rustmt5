# Types, Records, and Errors

## 1. `Value` และ `Record` (`protocol.rs`)

```rust
enum Value { Null, Bool(bool), Int(i64), UInt(u64), Float(f64), Str(String), Bytes(Vec<u8>), List(Vec<Value>), Record(Record) }
struct Record(IndexMap<String, Value>);   // Deref → IndexMap, เรียงตามลำดับ field
```

ชนิดที่ parse ออกมา: signed → `Int`, unsigned → `UInt`, float → `Float`, time → `Int`(unix ms), string → `Str`, bytes → `Bytes`,
บล็อกซ้อน (`tiers`, `trade`, `bids`, …) → `List`/`Record`

### Getter แบบให้อภัย (เลียนแบบ `d.get(k) or 0` ของ Python)

| เมธอด | ถ้า key ไม่มี / ชนิดไม่ตรง |
|---|---|
| `get_i64`, `get_u64` | `0` (u64: ค่าติดลบ → 0) |
| `get_f64` | `0.0` (รับ Int/UInt แปลงให้) |
| `get_bool` | `false` (ตัวเลข ≠ 0 = true) |
| `get_str` | `""` |
| `get_record`, `get_list`, `get_records` | `None` / `&[]` / ว่าง |

ข้อควรระวัง: **ไม่มี error เมื่อพิมพ์ชื่อ key ผิด** (ได้ 0 เงียบ ๆ). ใช้ `contains_key` / `rec.get("k")` เมื่อต้องแยก "ไม่มี" ออกจาก "ศูนย์"
และอ้างอิงชื่อ key จาก [reference/schemas.md](reference/schemas.md) (หรือ `*_FIELD_NAMES`)

สร้าง Record: `Record::new().with("symbol", "EURUSD").with("volume", 0.01)`; `Value: From<i*, u*, f*, bool, &str, String, Vec<u8>, Record, Vec<Record>, Vec<f64>>`

## 2. Struct ผลลัพธ์ (`types.rs`)

| ชนิด | ใช้กับ |
|---|---|
| `TradeResult` | ผล `trade_request` / helper ส่งออเดอร์; `Display` = `TradeResult(retcode=…, success=…, …)` |
| `AccountInfo` | `get_account_summary` |
| `SymbolInfo` | cache พื้นฐานของ symbol |
| `VerificationStatus`, `OpenAccountResult` | เปิดบัญชี (`OpenAccountResult::success()` = `code == 0`) |
| `AccountOpeningRequest` (`DemoAccountRequest` = alias), `RealAccountRequest{ base, … }`, `AccountDocument` | payload เปิดบัญชี; `Default` ตั้ง deposit 100000, leverage 100 |
| `TickEvent`, `BookEvent`, `TradeResultEvent`, `AccountEvent` | push แบบ typed (มี `raw: Record`) |
| `HealthStatus` | `health_check` |

## 3. `TimeArg`

อาร์กิวเมนต์เวลาที่รับ `i64/i32/u32/u64` (วินาที), `f64` (วินาทีทศนิยม), `SystemTime` ผ่าน `impl Into<TimeArg>`:

| ฟังก์ชัน | int วินาที | float | SystemTime |
|---|---|---|---|
| `coerce_timestamp` | ตามเดิม | ตัดทศนิยม | วินาทีตัดทศนิยม |
| `coerce_timestamp_ms` | `×1000` | `×1000` | `secs_f64×1000` |
| `coerce_timestamp_ms_end` | `×1000+999` | จำนวนเต็ม → `+999`; ไม่ใช่ → ms ปกติ | ms ปกติ |

`*_end` ใช้เป็นขอบบนแบบรวมของ `copy_ticks_range` (ครอบคลุมทั้งวินาที)

## 4. `Error` (`error.rs`)

| variant | เกิดเมื่อ | ตัวอย่าง |
|---|---|---|
| `Connection { message, server_uri }` | เปิด socket/bootstrap ไม่ได้, socket หลุดระหว่างรอคำตอบ | TLS ผิด, bootstrap code ≠ 0 |
| `Authentication(String)` | login `code ≠ 0` | `code=3` |
| `Trade { message, retcode, symbol, action }` | สงวนไว้สำหรับความผิดพลาดการเทรด (ปัจจุบัน `trade_request` คืน `TradeResult` แทน) | |
| `Protocol(String)` | frame/parse ผิด, ค่าเกินช่วง serialize | `frame length mismatch`, `buffer too short` |
| `SymbolNotFound(String)` | `subscribe_symbols` ชื่อไม่อยู่ใน cache | |
| `Validation(String)` | อินพุตไม่ผ่านการตรวจ | `volume must be > 0` |
| `Session(String)` | สถานะไม่ถูกต้อง | `transport not ready`, `transport closed` |
| `Timeout(String)` | command เกินเวลา | |
| `WebSocket(Box<tungstenite::Error>)`, `Io` | ชั้นล่าง | |

(`WebSocket` ถูก box เพื่อให้ `Error` เล็ก — clippy `result_large_err`)

### "Soft error" (`client::is_soft_error`)

Python จับ `KeyError/ValueError/RuntimeError/TypeError` ในหลาย helper แบบ best-effort. Rust แมปเป็น:
`Protocol | Validation | Session | Trade | SymbolNotFound` = soft → helper แปลงเป็น `None`/ค่า fallback + `last_error`;
`Timeout | Connection | WebSocket | Io | Authentication` = **propagate** (เหมือน Python ที่ไม่จับ `TimeoutError`/`ConnectionError`)

ใช้ใน: `find_conversion_symbol_name`, `detect_close_direction`, และ `order_calc_*`/`order_check`/`version` (เฉพาะ Protocol/Validation ตามต้นฉบับ)

## 5. Constants

`constants::*` — id ของ command (`u16`), `TRADE_ACTION_*`, `ORDER_TYPE_*`, `ORDER_FILLING_*`, `ORDER_TIME_*`, `POSITION_TYPE_*`, `DEAL_*`, `ORDER_STATE_*`
(เป็น `i64` เพื่อเทียบกับค่าใน `Record` ได้ตรง ๆ), `TRADE_RETCODE_*` (`u32`), `PERIOD_*` (`u16`), `*_CALC_MODES`, `VOLUME_PRECISION`
ส่วนที่เป็นเลขล้วนอยู่ใน `constants_gen.rs` (generate จาก Python ด้วย `tools/gen_constants.py`)
