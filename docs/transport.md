# Transport (`src/transport.rs`)

`Transport` ดูแล WebSocket ทั้งหมด: เปิด/ปิดการเชื่อมต่อ, handshake, เข้ารหัส, จับคู่ response กับ request, ส่ง push ให้ listener
`Transport` เป็น `Clone` (ภายใน `Arc<Inner>`) — recv task ถือสำเนาของตัวเอง

## 1. State machine

```
            connect()                 bootstrap OK
DISCONNECTED ───────► CONNECTING ──────────────────► READY
     ▲                    │ error                      │  socket หลุด/ปิดโดยเซิร์ฟเวอร์ (ไม่ได้สั่ง close)
     │ close()            ▼                            ▼
     └─────────────── ERROR ◄─────────────────────────┘
     ▲                                                 
     └──── CLOSING ◄── close() (ชั่วคราว) 
```

| State | ความหมาย | `send_command` |
|---|---|---|
| `Disconnected` | เริ่มต้น / ปิดแล้ว | `Error::Session("transport not ready …")` |
| `Connecting` | กำลังเปิด socket + bootstrap | ส่งได้เฉพาะ bootstrap ภายใน |
| `Ready` | พร้อมใช้ | ปกติ |
| `Closing` | `close()` กำลังทำงาน | ไม่พร้อม |
| `Error` | หลุดโดยไม่ตั้งใจ (recv task จบ) | ไม่พร้อม; `on_disconnect` ถูกเรียกแล้ว |

`is_ready()` = `state == Ready`

## 2. `connect()` ทีละขั้น

1. ถ้ามี socket เดิม → `close()` ก่อน (กัน double-connect)
2. ติดตั้ง rustls crypto provider (ring) — `install_default()` ซ้ำได้ ไม่ error
3. ตั้ง `Connecting`, ล้าง `shutdown`, รีเซ็ต cipher เป็น initial key และ token เป็นศูนย์ 64 ไบต์
4. สร้าง request พร้อม `Origin` header; เปิด WebSocket ภายใต้ `timeout` (error → `Error::Connection { message, server_uri }`)
5. `split()` เป็น sink/stream; เก็บ sink; **spawn recv task**
6. ส่ง bootstrap (cmd 0) ผ่าน `send_raw(check_ready = false)`
7. ตรวจ `code == 0` และ `body.len() >= 66`; เก็บ `token = body[2..66]`, `cipher = AesCipher::new(body[66..])`, `server_build = u16(body[0..2])`
8. ตั้ง `Ready`, เรียก `metrics.on_connect()`

ถ้าขั้นใดหลังขั้น 5 ล้มเหลว → `teardown()` (abort recv task, ปิด sink) และ state = `Error`

## 3. เส้นทางส่ง (`send_raw`)

```rust
validate cmd ∈ VALID_COMMANDS            // ไม่งั้น Error::Protocol("unsupported command")
if check_ready && !is_ready → Error::Session
rate_limiter.acquire().await
let (tx, rx) = oneshot::channel();
{
  let sink = self.sink.lock().await;      // send-lock: ส่งทีละเฟรม
  pending[cmd].push_back((id, tx));       // ลงทะเบียนก่อนส่ง เพื่อไม่ให้ response มาก่อน
  encrypt(build_command(cmd, payload)) → pack_outer → sink.send
}
timeout(self.timeout, rx).await
```

ผลลัพธ์ของ `rx`:
* `Ok(CommandResult)` → สำเร็จ
* `Failure::Closed` → `Error::Session("transport closed")`
* `Failure::Disconnected(msg)` → `Error::Connection`
* หมดเวลา → ลบ `(id, tx)` ออกจาก `pending[cmd]` แล้ว `Error::Timeout`
* ส่ง ws ไม่สำเร็จ → ลบ pending แล้ว `Error::WebSocket`

### ข้อควรระวัง: response มาช้าหลัง timeout

เพราะ response ไม่มี request id, response ที่มาช้าของคำสั่งที่ timeout ไปแล้ว **จะถูกจับคู่กับ waiter ตัวถัดไปของ command เดียวกัน**
(หรือถูกทิ้งถ้าไม่มี waiter). ในทางปฏิบัติ: ตั้ง `timeout` ให้พอ และอย่าส่ง command เดียวกันซ้อนกันเมื่อกำลังมี timeout เกิดขึ้น
(พฤติกรรมเดียวกับ pymt5)

## 4. recv task

```
while let Some(msg) = ws.next():
   Binary → handle_frame()      // unpack_outer → decrypt → parse_response_frame → dispatch
            ข้อผิดพลาด parse = log error แล้ว "ข้ามเฟรมนั้น" (ไม่ปิดการเชื่อมต่อ)
   Close  → break
   Err(e) → reason = e; break
   อื่น ๆ (Text/Ping/Pong) → ข้าม
หลุดจาก loop:
   ถ้า shutdown flag (เรา close เอง) → จบเงียบ ๆ
   ไม่งั้น: fail_all(Disconnected) · state=Error · metrics.on_disconnect · เรียก on_disconnect callback
```

> ต่างจาก Python: Python ถือว่าการปิดแบบปกติ (iterator จบ) ไม่ใช่ disconnect; Rust ถือว่าเซิร์ฟเวอร์ปิดเองเป็น disconnect เสมอ
> เพื่อให้ auto-reconnect ทำงานเมื่อเซิร์ฟเวอร์ตัดการเชื่อมต่อ

### dispatch

1. `metrics.on_command_received(cmd, code)`
2. ดึง waiter ตัวแรกที่ยัง "มีชีวิต" (`!tx.is_closed()`) จาก `pending[cmd]` แล้ว `send(Ok(result))`
3. clone รายการ listener ของ `cmd` ออกมา (นอก lock) แล้วเรียกทีละตัวใน `catch_unwind`
4. ถ้า listener panic → log + เรียก error handlers ทุกตัว (ที่ลงทะเบียนด้วย `add_callback_error_handler` / `Mt5Client::on_callback_error`);
   handler ที่ panic เองก็ถูกจับ

## 5. Listener API

```rust
let id: ListenerId = transport.on(CMD_TICK_PUSH, |r: &CommandResult| { /* sync, สั้น ๆ */ });
transport.off(CMD_TICK_PUSH, Some(id));   // ลบตัวเดียว
transport.off(CMD_TICK_PUSH, None);       // ลบทุกตัวของ command (รวม cache listener ของ client!)
```

* listener ถูกเรียก **ตามลำดับที่ลงทะเบียน**; `Mt5Client::new` ลงทะเบียน cache listener (tick/book) ก่อนเสมอ
  → handler ของผู้ใช้เห็น cache ที่อัปเดตแล้ว
* listener ทำงานบน recv task: ถ้า block จะหยุดรับข้อมูลทั้งหมด (รวม response ของ command อื่น)

## 6. Rate limiter (`rate_limiter.rs`)

Token bucket: `rate` token/วินาที, burst สูงสุด `burst`; `rate <= 0` = ปิด
`acquire()` คำนวณ token ใหม่ตามเวลาที่ผ่านไป ถ้าไม่พอจะ `sleep` นอก lock แล้ววนซ้ำ (cancel-safe)
ตั้งผ่าน `ClientConfig { rate_limit, rate_burst }`

## 7. TLS

* ใช้ rustls + provider **ring**; root certificates = `webpki-roots` (ฝังในไบนารี)
* **เพิ่ม** root CA ได้ผ่าน `ClientConfig::ca_bundle` (path ไฟล์ PEM) หรือตัวแปร `SSL_CERT_FILE` — เพิ่มความเชื่อถือเท่านั้น ไม่เคยปิด verification
  ใช้เมื่อเครือข่ายมี proxy ที่ re-terminate TLS (ใน sandbox ของการพัฒนาต้องใช้ `/root/.ccr/ca-bundle.crt`;
  ไม่ใส่จะได้ `invalid peer certificate: UnknownIssuer`)
* **ไม่รองรับ HTTP/SOCKS proxy ผ่าน env** (`HTTPS_PROXY`) — tokio-tungstenite เปิด TCP ตรง
  ถ้าต้องผ่าน proxy แบบ CONNECT ต้องเปิด stream เองแล้วใช้ `client_async_tls_with_config` (ยังไม่ได้ทำ)

## 8. Metrics

`MetricsCollector` (trait, ทุกเมธอดมี default ว่าง): `on_command_sent`, `on_command_received`, `on_connect`, `on_disconnect(reason)`,
`on_reconnect_attempt(n)`, `on_reconnect_success(n)` — ส่งผ่าน `ClientConfig.metrics: Option<Arc<dyn MetricsCollector>>`
