# Architecture

## 1. ชั้นของระบบ

```
┌────────────────────────────────────────────────────────────────────┐
│ Mt5Client  (src/client.rs + impl blocks ในไฟล์อื่น)                │
│   account · market_data · currency · trading · order_helpers ·     │
│   push_handlers       ← คือ "mixin" ของ Python กลายเป็น impl block │
├────────────────────────────────────────────────────────────────────┤
│ Transport  (src/transport.rs)                                      │
│   WebSocket · state machine · FIFO pending · listeners · rate limit│
├────────────────────────────────────────────────────────────────────┤
│ AesCipher  (src/crypto.rs)   AES-CBC, IV=0, PKCS7                  │
├────────────────────────────────────────────────────────────────────┤
│ protocol   (src/protocol.rs) framing + SeriesCodec + Value/Record  │
└────────────────────────────────────────────────────────────────────┘
        ↑ ใช้ร่วม: constants · schemas(generated) · parsers · helpers
```

ทิศทางการพึ่งพาเป็นบนลงล่างเท่านั้น: `Transport` ไม่รู้จัก `Mt5Client`
(การแจ้ง disconnect ทำผ่าน callback `set_on_disconnect`) ทำให้ทดสอบ transport ได้เดี่ยว ๆ

## 2. แผนผังไฟล์

| ไฟล์ | หน้าที่ | พอร์ตจาก (Python) |
|---|---|---|
| `lib.rs` | re-export สาธารณะ, doc หน้าแรก | `__init__.py` |
| `constants.rs`, `constants_gen.rs` | command id, order/trade constants, retcode text, calc-mode sets (gen = สร้างอัตโนมัติ) | `constants.py` |
| `schemas.rs` | schema + ชื่อ field ของทุก record (**generated**) | `schemas.py`, ส่วนหนึ่งของ `types.py` |
| `protocol.rs` | `PropType/Field`, `Value/Record`, `SeriesCodec`, framing | `protocol.py` |
| `helpers.rs` | hex, UTF-16LE, ASCII pad, client-id (SHA-1) | `helpers.py` |
| `crypto.rs` | `AesCipher`, initial key | `crypto.py` |
| `transport.rs` | `Transport`, `TransportState`, `CommandResult` | `transport.py` |
| `rate_limiter.rs` | token bucket | `_rate_limiter.py` |
| `metrics.rs` | trait `MetricsCollector` | `_metrics.py` |
| `error.rs` | enum `Error` | `exceptions.py` |
| `types.rs` | `TradeResult`, `AccountInfo`, `SymbolInfo`, request/event structs | `types.py`, `events.py` |
| `validation.rs` | validate_volume/price/symbol/state | `_validation.py` |
| `parsers.rs` | parse ตอบกลับ, tick normalisation, glob, validate order | `_parsers.py` |
| `client.rs` | `Mt5Client` core: config, login, heartbeat, reconnect, health | `client.py` |
| `account.rs` | บัญชี/รหัสผ่าน/เปิดบัญชี/OTP | `_account.py` |
| `market_data.rs` | symbols, ticks, book, rates, copy_* | `_market_data.py` |
| `currency.rs` | แปลงสกุลเงิน, สูตร profit/margin | `_currency.py` |
| `trading.rs` | positions/orders/history, trade_request, order_send/check | `_trading.py` |
| `order_helpers.rs` | buy_market … cancel_pending_order | `_order_helpers.py` |
| `push_handlers.rs` | `on_*`, cache tick/book | `_push_handlers.py` |
| `subscription.rs` | `SubscriptionHandle` | `_subscription.py` |
| `order_manager.rs` | `OrderManager` | `_order_manager.py` |
| `pool.rs` | `Mt5ConnectionPool` | `_pool.py` |

## 3. Data flow (หนึ่ง request/response)

```
caller ──► Mt5Client::get_account()
              │ build payload (SeriesCodec::serialize)
              ▼
          Transport::send_command(cmd, payload)
              │ 1 rate_limiter.acquire()
              │ 2 lock sink (serialise การส่ง)
              │ 3 push (id, oneshot::Sender) เข้า pending[cmd]   ← FIFO ต่อ command
              │ 4 inner = random(2)+cmd(u16)+payload → AES encrypt → pack_outer → ws.send
              │ 5 await oneshot ด้วย timeout
              ▼
        recv task (loop อ่าน ws)
              │ unpack_outer → decrypt → parse_response_frame
              │ dispatch: ปลุก waiter ตัวแรกของ pending[cmd]  + เรียก listeners[cmd] ทุกตัว
              ▼
          caller ◄── CommandResult { command, code, body }
              │ parse body (parsers.rs) → Record / struct
```

จุดสำคัญ: **response ไม่มี request id** เซิร์ฟเวอร์จับคู่ด้วย *เลข command* เท่านั้น
จึงต้องใช้คิว FIFO ต่อ command (คำสั่งเดียวกันสองตัวที่ส่งซ้อนกันจะได้คำตอบตามลำดับ)
และ push ที่ใช้ command เดียวกับ response (เช่น cmd 4, 14) จะถูกส่งให้ทั้ง waiter และ listeners

## 4. โมเดล concurrency

* ใช้ **Tokio** (multi-thread). `Mt5Client` เป็น `Clone` (ภายในเป็น `Arc<Inner>`) ทุก clone ใช้การเชื่อมต่อเดียวกัน
  จึงส่งเข้า `tokio::spawn` ได้โดยตรง
* task ที่มีตลอดอายุการเชื่อมต่อ:
  1. **recv task** (ใน `Transport`) — อ่าน WebSocket, decrypt, dispatch
  2. **heartbeat task** — `ping` ทุก `heartbeat_interval` (ถือ `Weak<Inner>` เพื่อไม่ยื้อ client ไว้)
  3. **reconnect task** — เกิดเมื่อหลุดและเปิด `auto_reconnect`
* **Listener/callback เป็น closure แบบ sync** รันบน recv task → ห้าม block, ห้าม `.await`
  งานหนักให้ส่งผ่าน channel (`tokio::sync::mpsc`) หรือ `tokio::spawn` เอง
  ถ้า callback panic จะถูก `catch_unwind` จับ แล้วแจ้ง `on_callback_error` (recv task ไม่ตาย)

## 5. กฎการ lock (สำคัญเมื่อแก้โค้ด)

| ตัวแปร | ชนิด | กฎ |
|---|---|---|
| `Inner.state` (cache ทั้งหมดของ client) | `std::sync::Mutex<State>` | ถือสั้น ๆ **ห้ามถือข้าม `.await`** ใช้ผ่าน `self.st()` ปล่อยก่อนเรียก callback/await |
| `Transport.sink` | `tokio::sync::Mutex` | ถือขณะส่งเฟรมเท่านั้น (ทำหน้าที่ send-lock ด้วย) |
| `Transport.pending / listeners` | `std::sync::Mutex` | ถือเฉพาะตอน push/pop; เรียก listener **นอก** lock (clone `Arc` ออกมาก่อน) |
| `Transport.cipher` | `std::sync::RwLock` | เปลี่ยนค่าเฉพาะตอน connect (หลัง bootstrap) |

push handler ที่ต้องใช้ cache จะจับ `Arc<Mutex<State>>` (ไม่ใช่ตัว client) เพื่อไม่ให้เกิด reference cycle
`Mt5Client → Transport → listener → Mt5Client`. ส่วน disconnect callback ใช้ `Weak<Inner>` ด้วยเหตุผลเดียวกัน

## 6. ความต่างเชิงโครงสร้างจาก Python

| Python | Rust | เหตุผล |
|---|---|---|
| mixin classes | หลาย `impl Mt5Client` ในไฟล์แยก | ภาษาไม่มี inheritance; แยกไฟล์ให้ขนาดดูแลง่ายเหมือนเดิม |
| `dict` ทุกที่ | `Record` + struct สำหรับ dataclass | คง semantics ของต้นฉบับ แต่ได้ type สำหรับผลลัพธ์ระดับบน |
| exception (หลายชนิด + multi-inherit) | `enum Error` + `Result` | ดู [types-and-errors.md](types-and-errors.md) |
| `async with client` | `connect()` / `close()` ตรง ๆ | ไม่มี async `Drop` |
| callback sync/async | closure sync | ตัดความซับซ้อน; ดูข้อ 4 |
| reconnect สร้าง Transport ใหม่ | ใช้ Transport เดิม | listener ไม่หาย (ดู [client.md](client.md)) |
