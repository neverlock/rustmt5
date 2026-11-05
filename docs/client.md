# Client core (`src/client.rs`)

## 1. `ClientConfig`

| field | default | ความหมาย |
|---|---|---|
| `uri` | `wss://web.metatrader.app/terminal` | WebSocket URL |
| `timeout` | 30.0 | วินาที — ทั้ง connect และแต่ละ command |
| `heartbeat_interval` | 30.0 | ระยะ `ping` ของ heartbeat task |
| `tick_history_limit` | 10 000 | จำนวน tick ที่เก็บต่อ symbol (0 = ไม่จำกัด) |
| `max_tick_symbols` | 0 | จำนวน symbol สูงสุดที่เก็บ history (0 = ไม่จำกัด; เกินแล้วลบตัวที่ update นานสุด = LRU) |
| `auto_reconnect` | false | เปิด reconnect อัตโนมัติ |
| `max_reconnect_attempts` | 5 | |
| `reconnect_delay` / `max_reconnect_delay` | 3.0 / 60.0 | ฐานและเพดานของ backoff |
| `rate_limit` / `rate_burst` | 0 / 20 | token bucket |
| `metrics` | None | |
| `symbol_cache_ttl` | 0 | วินาที; 0 = `load_symbols` ดึงใหม่ทุกครั้ง |
| `ca_bundle` | None | PEM root CA เพิ่มเติม (ดู [transport.md](transport.md)) |

`ClientConfig::new(uri)` ตั้ง uri และใช้ค่าอื่นเป็น default

## 2. `State` (cache ภายใน)

`Inner.state: Arc<Mutex<State>>` เก็บ:

| field | ใช้ทำ |
|---|---|
| `symbols` / `symbols_by_id` / `symbols_loaded_at` | cache จาก `load_symbols`; ใช้ map ชื่อ↔id, normalise ราคา tick |
| `full_symbols` | cache ผล cmd 18 ต่อชื่อ (ล้างเมื่อ `load_symbols` โหลดใหม่) |
| `tick_cache_by_id/name` | tick ล่าสุด → `symbol_info_tick`, แปลงสกุลเงิน, merge ค่า 0 ("ไม่เปลี่ยน") |
| `tick_history_by_id` + `tick_history_access_order` | ประวัติ tick (VecDeque) + ลำดับ LRU → `copy_ticks_*` |
| `book_cache_by_id/name` | DOM ล่าสุด → `market_book_get` |
| `subscribed_ids`, `subscribed_book_ids` | ชุดที่ subscribe อยู่ (ใช้ซ้ำตอน reconnect) |
| `logged_in`, `bootstrap_pristine`, `closing` | แฟล็กสถานะ |
| `login_params` | เก็บข้อมูลล็อกอินไว้ reconnect (ลบเมื่อ `close()`) |
| `last_error` | `(code, message)` ของ helper แบบ MetaTrader5 (`last_error()`) |
| `reconnect_count`, `connected_at`, `health_threshold_ms` | health |

> Python เก็บ tick history สองชุด (by id / by name ชี้ deque เดียวกัน); Rust เก็บชุดเดียวตาม id แล้วหาจากชื่อผ่าน `symbols`

## 3. Login flow

```
connect()           Transport::connect → bootstrap_pristine = true, connected_at = now
   │
[init_session()]    cmd 29 (ไม่บังคับ) — bootstrap_pristine = false
   │
login(login, pw)    = login_with(LoginParams::new(login, pw))
   ├ build_login_payload  (912 B)
   ├ send cmd 28
   ├ code ≠ 0 → Err(Authentication("login rejected by server: code=N …"))
   ├ parse LOGIN_RESPONSE → (token_hex, session_id)
   ├ logged_in = true; เก็บ LoginParams (สำหรับ reconnect)
   └ ถ้า auto_heartbeat → start_heartbeat()
```

`LoginParams` มี `Debug` ที่ **ซ่อนรหัสผ่าน** (`<redacted>`) — อย่าเปลี่ยนเป็น derive

`is_connected()` = `transport.is_ready() && logged_in`

### `bootstrap_pristine` และ cmd 52

บน build 5687 `cmd 52` ปลอดภัยเฉพาะ "ก่อน init/login" (ไม่งั้นเซิร์ฟเวอร์ตัด socket)
`bootstrap_pristine` = true หลัง connect และเป็น false เมื่อมี `init_session`/`login`/`send_raw_command(28|29|52)`
`send_bootstrap_command_52` ปฏิเสธถ้าไม่ pristine

## 4. Heartbeat

`start_heartbeat` spawn task: วน `sleep(heartbeat_interval)` → `ping()`; ล้มเหลวแค่ log warning
task ถือ `Weak<Inner>` — เมื่อ client ถูก drop ทั้งหมด task จะจบเอง; `stop_heartbeat` (ตอน logout/close/disconnect) `abort()`

## 5. Reconnect

เริ่มเมื่อ transport แจ้ง disconnect, `auto_reconnect = true`, ไม่ได้อยู่ระหว่าง `close()`, และมี `login_params`:

```
handle_disconnect():  logged_in=false · หยุด heartbeat · เรียก on_disconnect(user) · spawn reconnect_loop (ถ้ายังไม่มี task วิ่งอยู่)

reconnect_loop():
  for attempt in 1..=max_reconnect_attempts:
      delay = min( base·2^(attempt-1) + U(0, base),  max_delay )
      metrics.on_reconnect_attempt; sleep(delay)
      try:  transport.close(); transport.connect()
            login_with(stored params, auto_heartbeat=true)
            subscribe_ticks(subscribed_ids) ; subscribe_book(subscribed_book_ids)
            reconnect_count += 1; connected_at = now; metrics.on_reconnect_success; return
      catch: log warning แล้วลองรอบถัดไป
  หมดรอบ → log error (ไม่มี callback ล้มเหลวสุดท้าย — ดู on_disconnect/health_check)
```

**ต่างจาก Python:** reuse `Transport` เดิมจึง *ไม่เสีย listener* (รวม cache tick/book และ handler ของผู้ใช้).
Python สร้าง transport ใหม่ ทำให้ listener หายหลัง reconnect (มี integration test
`disconnect_callback_and_auto_reconnect_restores_subscriptions` ครอบคลุมพฤติกรรมนี้)

ข้อจำกัด: ระหว่าง reconnect, command ที่กำลังรอจะได้ `Error::Connection` ทันที (fail_all)

## 6. `close()`

1. `closing = true` (กัน reconnect), หยุด heartbeat, abort reconnect task
2. ถ้า `logged_in` → `logout()` (ล้มเหลวก็ข้าม) 
3. `transport.close()` (fail command ที่ค้างด้วย `Error::Session("transport closed")`)
4. ล้าง `login_params` (ลบข้อมูลรหัสผ่านออกจากหน่วยความจำ), `closing = false`

## 7. Health

`health_check()` → `HealthStatus { state, ping_latency_ms, last_message_age_secs, uptime_seconds, reconnect_count }`
ถ้า transport พร้อมจะ `ping` วัด latency; ถ้า latency > threshold (ตั้งด้วย `on_health_degraded(cb, threshold_ms)`, default 5000) เรียก callback ที่ลงทะเบียนไว้

## 8. `last_error` (ความเข้ากันกับ MetaTrader5)

helper แบบ official-style ที่ "ล้มเหลวแบบนุ่มนวล" (`order_calc_profit`, `order_calc_margin`, `order_check`, `version`) คืน `None`/retcode และเก็บ `(code, message)`
ไว้ให้อ่านด้วย `last_error()` — code ที่ใช้: `-1` action ไม่รองรับ, `-2` ไม่พบ symbol, `-3` ไม่มีสกุลเงินบัญชี, `-4` ไม่มี currency_profit,
`-5/-6` แปลงสกุลเงินไม่ได้ (profit/margin), `-7` ไม่มี build, `-10…-18` ข้อมูลสูตรไม่พอ (ดู [trading.md](trading.md)), `-99` parse/validation ผิดพลาด

## 9. Escape hatches

* `send_raw_command(cmd, payload)` — ส่ง command ใด ๆ ใน `VALID_COMMANDS` (ใช้ลองคำสั่งใหม่ได้)
* `transport()` — เข้าถึง `Transport` เพื่อ `on/off` listener ดิบ
