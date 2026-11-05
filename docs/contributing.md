# Contributing / Maintenance guide

## 1. Workflow

```bash
cargo fmt           # (ถ้าต้องการ) — โค้ดปัจจุบันใช้บรรทัดยาวได้ ไม่ได้บังคับ rustfmt
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

* branch เริ่มต้นของรีโปคือ `master`
* ห้าม commit ข้อมูลล็อกอิน/รหัสผ่านจริง; ตัวอย่าง/เทสต์ต้องอ่านจาก ENV และไม่พิมพ์รหัสผ่าน

## 2. สูตร (recipes)

### 2.1 เพิ่ม command ใหม่ที่ "ถามแล้วตอบ"

1. ถ้ายังไม่มีใน `VALID_COMMANDS` (`constants.rs`) ให้เพิ่มเลข (และ constant ใน Python → regenerate `constants_gen.rs`; หรือเพิ่ม `CMD_*` ตรง ๆ ถ้าไม่ได้มาจาก pymt5)
2. ถ้า body เป็นโครงสร้าง record: เพิ่ม schema ใน pymt5 `schemas.py` แล้ว regenerate (2.4) *หรือ* เขียน `const X_SCHEMA: &[Field]` ใน `schemas.rs`
   ชั่วคราว (จะถูกเขียนทับถ้า regenerate — ควรย้ายเข้า generator)
3. เขียนเมธอดใน `impl Mt5Client` ของไฟล์ที่เหมาะสม:
   ```rust
   pub async fn get_foo(&self, id: u32) -> Result<RecordList> {
       let payload = SeriesCodec::serialize(&[SerField::u32(id)])?;
       let r = self.transport().send_command(CMD_GET_FOO, &payload).await?;
       Ok(parse_counted_records(&r.body, FOO_SCHEMA, FOO_FIELD_NAMES))
   }
   ```
4. ตรวจ `r.code` ถ้าเซิร์ฟเวอร์ใช้ `code ≠ 0` แทน error (เช่น cmd 18) — อย่าปล่อยให้ parse body ว่างแล้วได้ error สับสน
5. เพิ่มเทสต์: parser (unit) + arm ใน mock (integration) + แถวใน [reference/commands.md](reference/commands.md)

### 2.2 เพิ่ม push handler

1. เขียน parser ใน `parsers.rs`/`push_handlers.rs` (`fn(&[u8]) -> Record`, ไม่ panic: ใช้ `get(..)`/`read_u32` แทน index ตรง)
2. ```rust
   pub fn on_foo(&self, cb: impl Fn(&Record) + Send + Sync + 'static) -> ListenerId {
       self.transport().on(CMD_FOO_PUSH, move |r| cb(&parse_foo(&r.body)))
   }
   ```
3. ถ้าต้องใช้ cache ของ client ให้จับ `self.inner.state.clone()` (ไม่ใช่ `self.clone()`) แล้ว lock สั้น ๆ ภายใน closure
4. callback ห้าม block; ถ้าต้อง cache ให้เพิ่มใน `install_cache_listeners` (ลงทะเบียนก่อน handler ผู้ใช้)

### 2.3 เพิ่ม/เปลี่ยน field ของ record

* แก้ที่ **ต้นทาง Python** (`schemas.py`) แล้ว regenerate เท่านั้น; ตรวจว่า `len(schema) == len(names)` (generator ไม่ตรวจ — ตรวจเองหรือให้เทสต์ขนาด schema จับ)
* เปลี่ยนความยาว schema = เปลี่ยน `get_series_size` → ตรวจเทสต์ขนาดใน `protocol.rs` และ doc ตาราง

### 2.4 Regenerate โค้ดจาก pymt5

```bash
git clone https://github.com/cloudQuant/pymt5 /tmp/pymt5     # หรือ path อื่น
python3 -I tools/gen_schemas.py      /tmp/pymt5 src/schemas.rs
python3 -I tools/gen_constants.py    /tmp/pymt5 src/constants_gen.rs
python3 -I tools/gen_schema_docs.py  /tmp/pymt5 docs/reference/schemas.md
```
สคริปต์ใช้ `ast` อ่านซอร์ส **โดยไม่ execute** — ห้ามเปลี่ยนเป็น `import pymt5` (โค้ดภายนอก)
generator รองรับเฉพาะ literal / ชื่อค่าคงที่ / `list + list` ถ้า pymt5 เปลี่ยนรูปแบบ อาจต้องปรับ `ev()`
(`constants.rs` ที่เขียนมือ — `VALID_COMMANDS`, ตาราง retcode, calc-mode sets — ต้องอัปเดตเองถ้าต้นทางเปลี่ยน)

### 2.5 เพิ่ม option ให้ `ClientConfig`

เพิ่ม field + `Default` (ใน `client.rs`) → ใช้ค่าใน `Mt5Client::new`/`Transport::with_options` → เพิ่มบรรทัดในตาราง [client.md](client.md)

## 3. Conventions

* ฟังก์ชันที่คุยกับเซิร์ฟเวอร์คืน `Result<T>`; helper แบบ MetaTrader5 ที่ "ล้มเหลวแบบนุ่มนวล" คืน `Result<Option<T>>` + `fail_last_error`
* **ห้ามถือ `std::sync::MutexGuard` (เช่น `self.st()`) ข้าม `.await`** — ถ้า future ต้องเป็น `Send` (เช่น `tokio::spawn`) คอมไพเลอร์จะ error ให้;
  แต่ถ้าไม่ได้ spawn จะผ่านคอมไพล์แล้ว deadlock ตอนรัน จึงให้ปล่อย guard (block `{ … }` สั้น ๆ) ก่อน `.await` เสมอ
* ความยาวข้อความที่ส่งให้ตัดแบบอักขระด้วย `trunc` ก่อน แล้วค่อย encode ลง field ไบต์
* ค่าคงที่ enum ของโปรโตคอลเป็น `i64` (เทียบกับ Record) ส่วน field ที่ serialize ใช้ `SerField::u32(…)` หรือ `SerField::new(PropType::U32, i64, None)` (ตรวจช่วง)
* log ใช้ `tracing` (`debug!/info!/warn!/error!`); **อย่า log รหัสผ่าน/token**

## 4. ความต่างจาก pymt5 (สรุป)

| หัวข้อ | pymt5 | rustmt5 | เหตุผล |
|---|---|---|---|
| reconnect | transport ใหม่ → listener หาย | transport เดิม | ไม่เสีย callback/cache |
| login code | ไม่ตรวจ | `Error::Authentication` | error ที่อ่านรู้เรื่อง |
| socket ปิดโดยเซิร์ฟเวอร์ (ปกติ) | ไม่ถือว่า disconnect | ถือว่า disconnect | ให้ auto-reconnect ทำงาน |
| cmd 18 request | ชื่อ symbol | **symbol id** (+fallback ชื่อ) | build 6182 ปฏิเสธชื่อ |
| bars layout กำกวม | เลือก 56 B เสมอ | ตรวจ timestamp เลือก 48 B ก่อน | ได้ค่าขยะจริง |
| `order_check` volume | เทียบ lot กับ wire volume | แปลง wire→lot ก่อน | ผิดหน่วยจริง |
| conversion symbol | `startswith(base)&endswith(quote)` | + `startswith(base+quote)` | broker มี suffix (`EURUSDp`) |
| `order_check` margin ไม่ทราบ | เงียบ (0) | `margin_known=false` | ผลผ่านที่ไม่แน่นอน |
| callback | sync/async | sync closure | ภาษา |
| pandas, `PYMT5_*` env | มี | ไม่มี | ไม่เกี่ยวข้อง |

## 5. Quirks ของ pymt5 ที่คงไว้โดยตั้งใจ

* trade-settings count = 0 ไม่ถูกข้าม (ดู [account-and-push.md](account-and-push.md))
* `normalize_tick_price_value` heuristic (ดู [market-data.md](market-data.md))
* tick merge ใช้ cache ข้าม batch เท่านั้น
* `positions_get/orders_get` ใช้ตัวกรองแรกที่ให้มาเท่านั้น
* response ที่มาช้าหลัง timeout จับคู่ผิดคำสั่ง (ไม่มี request id)
* deal เวลาเป็น ms ส่วน position/order เป็นวินาที

## 6. ข้อเสนอแนะ / งานที่ยังไม่ได้ทำ

เรียงตามความน่าจะส่งผลต่อผู้ใช้:

1. **margin/margin_free ของบัญชี** — cmd 3 ไม่มี; ควรสะสมจาก push cmd 10 (balance update) หรือคำนวณจาก positions เพื่อให้ `order_check` แม่นขึ้น
2. **ตัวเลือกปิด tick price normalisation** (`ClientConfig.normalize_ticks: bool`) — heuristic เสี่ยงทำราคาหุ้น/ทองผิด (ดู market-data.md)
3. **ทดสอบการส่งออเดอร์จริงบน demo** และยืนยัน push cmd 10/19 กับเซิร์ฟเวอร์จริง (ตอนนี้ mock เท่านั้น)
4. **Proxy แบบ CONNECT** สำหรับ WebSocket (ปัจจุบันเปิด TCP ตรง)
5. ความละเอียด volume ต่อ broker (ตอนนี้ฮาร์ดโค้ด 10^8) — อาจอ่านจากข้อมูล symbol/บัญชี
6. `Error::Trade` ยังไม่ถูกใช้โดย `trade_request` (คืน `TradeResult` เหมือน pymt5) — ถ้าต้องการ API แบบ error-on-failure ให้เพิ่ม wrapper
7. reconnect: callback เมื่อหมดรอบ (`on_reconnect_failed`) ยังไม่มี
