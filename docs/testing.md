# Testing

## 1. รันเทสต์

```bash
cargo test --all-targets          # unit (33) + integration กับ mock server (7)
cargo test --doc                  # doctest
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps               # rustdoc
```

CI (`.github/workflows/ci.yml`) รัน clippy (deny warnings) + test + doctest

## 2. Unit tests (ในแต่ละไฟล์ `#[cfg(test)]`)

| ไฟล์ | ครอบคลุม |
|---|---|
| `helpers.rs` | hex, UTF-16LE round-trip (รวมภาษาไทย), ASCII pad, client id |
| `crypto.rs` | ความยาว initial key = 32, round-trip ทุกขนาด key, ขนาดแพ็กเก็ต bootstrap ตรง browser capture |
| `protocol.rs` | ขนาด schema (526/50/48/344/356/356), outer frame, response frame, FILETIME, round-trip, ค่าเกินช่วง |
| `parsers.rs` | glob/group mask, timestamp coercion, tick normalisation + cache merge, bars (รวม layout กำกวม), counted records, book, volume/stops validation (รวมการแปลง wire→lots), open-account, account response |
| `validation.rs`, `rate_limiter.rs`, `order_manager.rs`, `account.rs` | ตามชื่อ |

เทสต์ที่ "ล็อกพฤติกรรมของ pymt5" โดยตั้งใจ: `tick_price_normalisation` (เช่น `2000.0, digits=2 → 20.0`), `account_response_derived_fields`

## 3. Integration tests กับ mock server (`tests/mock_server.rs`)

mock รันใน process เดียวกัน (`TcpListener` + `tokio_tungstenite::accept_async`) และ **พูดโปรโตคอลจริง**: ถอดรหัสด้วย initial key,
ตอบ bootstrap (ส่ง session key 32 B แล้วสลับ cipher) จากนั้นจัดการ command ตาม `match cmd`:

| cmd | mock ตอบ |
|---|---|
| 0 | `build 6182 ‖ token ‖ key` |
| 28 | ตรวจ payload = 912 B; ตอบ token+session 777 (หรือ code 3 ถ้า `reject_login`) |
| 3 | account (balance 1000.5, USD, 1:100, demo) + trade settings 1 รายการ |
| 34 | symbols 2 ตัว (EURUSD id1 digits5, USDJPY id2) บีบอัด zlib |
| 7 | บันทึกชุด id; ตอบ ack แล้ว push tick 2 เฟรม (ราคา scaled, ตัวที่สอง bid=0) |
| 18 | ตรวจรูป `[n][ids]`; ถ้าไม่ใช่ตอบ `code=1` (จำลองพฤติกรรม build 6182); ตอบ full symbol พร้อม `trade` (volume min/step/max แบบ wire) |
| 4 | positions/orders ว่าง |
| 12 | trade response retcode 10009 |
| อื่น ๆ | ack ว่าง; `ignore_cmd` = ไม่ตอบ (ทดสอบ timeout) |

`Behavior` ปรับพฤติกรรมได้: `reject_login`, `drop_after_login_once` (ปิด socket หลัง login ครั้งแรก), `ignore_cmd`, ตัวนับ `logins/connections`, รายการ `subscribes`

| เทสต์ | พิสูจน์อะไร |
|---|---|
| `handshake_login_ping_logout` | handshake เข้ารหัส, login, ping, health, close |
| `login_rejection_is_authentication_error` | `code=3` → `Error::Authentication` |
| `account_symbols_ticks_and_trade` | parse account/symbols/ticks (normalise + carry-forward), subscribe merge, managed handle, full symbol, **`order_check` หน่วย volume/margin/no-money**, ส่งออเดอร์ (mock) |
| `command_timeout_and_transport_not_ready` | `Error::Session` ก่อน connect, `Error::Timeout` แล้ว connection ยังใช้ต่อได้ |
| `disconnect_callback_and_auto_reconnect_restores_subscriptions` | หลุด → callback → reconnect → re-login → listener ยังอยู่ |
| `panicking_push_callback_does_not_kill_receive_loop` | panic ใน callback ถูกจับ ไม่ฆ่า recv task |
| `login_payload_matches_browser_capture_sizes` | ขนาดเฟรม login 916/928/936 |

### เขียนเทสต์ใหม่กับ mock

1. เพิ่ม arm ใน `serve_connection` สำหรับ command ที่ต้องการ (ใช้ `frame(&cipher, cmd, code, &body)` ส่งกลับ)
2. สร้าง body ด้วย `record_bytes(SCHEMA, FIELD_NAMES, &[("field", Value::…)])` — field ที่ไม่ระบุเป็น 0/ว่าง
3. ถ้าต้องการ push ให้ `ws.send` เฟรมเพิ่มหลังตอบ
4. อย่าลืมกฎ: **ต้องตอบด้วย command id เดียวกับ request** (transport จับคู่ด้วย command)

## 4. Live check (เซิร์ฟเวอร์จริง)

```bash
LOGIN=123456 PASSWORD=... SERVER=wss://host/terminal cargo run --example live_check
```

ทำเฉพาะการอ่าน: connect → login → account → symbols → groups → positions/orders → history → full symbol info → bars H1 5 วัน → subscribe tick 8 วินาที → health → close.
**ไม่ส่งออเดอร์ และไม่พิมพ์รหัสผ่าน.** ตัวอย่างผล (build 6182, บัญชีจริงยอดเล็ก): symbols 968, groups 13, history deals 3 053, ping ≈ 106 ms

### ข้อควรระวัง (เรียนรู้จากการพัฒนา)

* **อย่า login ซ้ำรัว ๆ เมื่อได้ `code=3`** — อาจทำให้บัญชีโดนล็อก ตรวจข้อมูลก่อน
* `#` ใน ENV/`.env` มักถูกตัดเป็น comment → รหัสผ่านสั้นกว่าจริง (ตรวจด้วยความยาว ไม่ต้องพิมพ์ค่า)
* ENV ถูกอ่านตอนเริ่ม session — แก้ระหว่างทางไม่มีผลกับ process ที่รันอยู่
* เครือข่ายที่ re-terminate TLS ต้องชี้ `SSL_CERT_FILE`/`ClientConfig::ca_bundle` ไปที่ CA bundle (ไม่ใช่ปิด verification)
* ตลาดปิด = ไม่มี tick (ทดสอบ tick ด้วย crypto ที่เปิด 24/7 เช่น `BTCUSDp`)
* อย่า commit รหัสผ่าน/login — `.gitignore` มี `.env` แล้ว; ก่อน commit ควร `grep -rF "$PASSWORD" .` ว่าไม่พบ

## 5. สิ่งที่ยังไม่ได้ทดสอบกับเซิร์ฟเวอร์จริง

ส่งออเดอร์/แก้/ยกเลิก, order book (DOM), push cmd 10/13/14/15/17/19, spreads, เปิดบัญชี, เปลี่ยนรหัส, OTP, cmd 6 (ไม่บีบอัด), cmd 52
(ดูสถานะรายคำสั่งใน [reference/commands.md](reference/commands.md))
