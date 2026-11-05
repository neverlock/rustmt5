# Protocol

โปรโตคอลนี้ได้จากการ reverse-engineer JS bundle ของ MT5 Web Terminal (โดย pymt5) เป็น binary over WebSocket
ทุกตัวเลขเป็น **little endian**

## 1. การเชื่อมต่อ

* URL ตัวอย่าง: `wss://web.metatrader.app/terminal` (broker white-label ใช้ path อื่น เช่น `wss://host/terminala`)
* ต้องส่ง header `Origin: https://web.metatrader.app`
* ไม่จำกัดขนาด message (`max_message_size = None`) เพราะรายการ symbols/ประวัติใหญ่
* **ไม่** ใช้ WebSocket ping (ใช้ cmd 51 เป็น heartbeat ระดับแอป)
* ใช้ binary message เท่านั้น (text message ถูกข้าม)

## 2. Outer frame

```
 0        4        8
 ├────────┼────────┼──────────────────────────
 │ len u32│ ver u32│ body (len bytes, = ciphertext)
 └────────┴────────┴──────────────────────────
```

* `len` = ความยาว body, `ver` = `OUTER_PROTOCOL_VERSION` (= 1)
* `pack_outer(body)` / `unpack_outer(frame)` ใน `protocol.rs`; `unpack_outer` ตรวจ `len == body.len()` มิฉะนั้น `Error::Protocol("frame length mismatch …")`

## 3. Inner frame (หลัง decrypt)

**Request** (ไคลเอนต์ → เซิร์ฟเวอร์):

```
 0     2       4
 ├─────┼───────┼───────────
 │rand2│cmd u16│ payload
 └─────┴───────┴───────────
```
`rand2` = 2 ไบต์สุ่ม (`build_command`) — เซิร์ฟเวอร์ไม่ใช้ แต่ต้องมี

**Response/push** (เซิร์ฟเวอร์ → ไคลเอนต์):

```
 0     2       4      5
 ├─────┼───────┼──────┼──────────
 │ ??  │cmd u16│code  │ body
 └─────┴───────┴──────┴──────────
```
* `code` = 0 สำเร็จ; ≠ 0 คือ error ระดับคำสั่ง และ `body` มักว่าง
  (login ผิด → `code=3`, cmd 18 รูปแบบผิด → `code=1`)
* ต้องมีอย่างน้อย 5 ไบต์ ไม่งั้น `Error::Protocol("response frame too short")`

## 4. การเข้ารหัส

* AES-CBC, **IV = 16 ไบต์ศูนย์**, padding PKCS7 (`AesCipher`)
* ก่อน handshake ใช้ *initial key* (32 ไบต์) ที่ฝังเป็นสตริง obfuscate ใน `INITIAL_KEY_OBFUSCATED`
  decode ด้วย `obfuscation_decode` (ลบ 1 จากแต่ละ char; code 28 → `&`, 23 → `!`) แล้ว hex→bytes
* หลัง bootstrap สำเร็จใช้ *session key* ที่เซิร์ฟเวอร์ส่งมา (ความยาว 16/24/32; เซิร์ฟเวอร์จริงที่ทดสอบส่ง 32)

## 5. Handshake (bootstrap, cmd 0)

```
Client                                          Server
  │  cmd 0, payload = token(64 bytes of 0)  ──►   │   เข้ารหัสด้วย initial key
  │                                               │
  │  ◄── code 0, body = build(u16) ‖ token(64) ‖ session_key(16|24|32)
  │      เข้ารหัสด้วย initial key
  │  เปลี่ยนไปใช้ session_key; เก็บ token; server_build = build
  │
  │  (ตัวเลือก) cmd 29 init_session
  │  cmd 28 login …
```

ขนาดที่ยืนยันกับ browser capture (ใช้เป็น regression test):
bootstrap inner 68 B → encrypted 80 B → framed 88 B; login payload 912 B → inner 916 → enc 928 → framed 936;
init payload 744 B → 748 → 752 → 760

## 6. Primitive types (`PropType`)

| PropType | id | wire | Rust `Value` ที่ parse ได้ | หมายเหตุ |
|---|---|---|---|---|
| I8/I16/I32/I64 | 1/2/3/17 | 1/2/4/8 B signed | `Int(i64)` | |
| U8/U16/U32/U64 | 4/5/6/18 | 1/2/4/8 B unsigned | `UInt(u64)` | |
| F32 / F64 | 7 / 8 | IEEE-754 | `Float(f64)` | |
| Time | 9 | 8 B **Windows FILETIME** | `Int(unix_ms)` | `ft = (unix_ms + 11_644_473_600_000) × 10_000` |
| String | 10 | N B latin-1, NUL pad | `Str` | ใช้น้อย |
| FixedString | 11 | N B **UTF-16LE**, NUL pad | `Str` | N เป็นไบต์ → ตัวอักษร = N/2; ตัดที่ NUL ตัวแรก |
| Bytes | 12 | N B ดิบ | `Bytes` | |

กฎ serialize (`SeriesCodec::serialize`):
* จำนวนเต็มเกินช่วง → `Error::Protocol("U32 value out of range …")` (ไม่ wrap เงียบ ๆ)
* `FixedString` ยาวเกิน → ตัดที่ N ไบต์ (อาจตัดกลาง surrogate — พฤติกรรมเดียวกับ Python)
* `Bytes` ที่มี `len` → pad/ตัดให้ครบ; ไม่มี `len` (`SerField::raw`) → ต่อท้ายตามจริง
* ความยาวข้อความ "ตัดก่อน" ระดับอักขระใน client (`trunc(s, 32)` เป็นต้น) แล้วค่อย encode ลง field ที่กว้างเป็น 2 เท่า

`SeriesCodec::parse(buf, schema, offset)` ตรวจ `buf.len() - offset >= get_series_size(schema)` ก่อนเสมอ
(error `buffer too short: need N bytes from offset O, but only M bytes available`)

## 7. รูปแบบ body ที่พบบ่อย

| รูปแบบ | ใช้กับ | parser |
|---|---|---|
| `[u32 count][record]*` | symbols, positions, orders, spreads, corporate links, full symbols … | `parsers::parse_counted_records` |
| record เดียว ไม่มี count | account, trade response | `SeriesCodec::parse` |
| `[u32 count][u32 id]*` | request ของ subscribe ticks/book, get_spreads, **get_full_symbols (cmd 18)** | `market_data::id_list_payload` |
| บล็อกต่อกัน (positions แล้วต่อด้วย orders) | cmd 4 | `push_handlers::split_positions_orders` |
| ไม่มี count, `len / record_size` | tick push, rate bars, symbol details | `parse_tick_batch`, `parse_rate_bars` |
| `[4 B header][zlib stream]` | symbols แบบบีบอัด (cmd 34) | `get_symbols` (ตัด 4 ไบต์แรก แล้ว `ZlibDecoder`) |

ทุก parser ที่เป็น "list" **หยุดอย่างเงียบ ๆ** เมื่อ body สั้นกว่าที่ count อ้าง (คืนเท่าที่อ่านได้)
เพื่อทนต่อ server build ที่ต่างกัน — ถ้าข้อมูลหาย ให้ตรวจ `body.len()` กับขนาด schema ก่อน

## 8. Login payload (cmd 28) — 912 ไบต์

| offset | size | field | ค่า |
|---|---|---|---|
| 0 | 4 | version u32 | `LoginParams.version` (0 ใช้ได้) |
| 4 | 64 | password (UTF-16LE, 32 ตัวอักษร) | ตัดที่ 32; ว่างถ้ารหัสเป็น blob 320 hex |
| 68 | 128 | otp | ตัดที่ 64 ตัวอักษร |
| 196 | 16 | client id | SHA-1 ของ `platform;dpr;lang;screen;uniq` 16 ไบต์แรก (หรือ `cid` ที่ส่งมา) |
| 212 | 64 | utm_campaign | |
| 276 | 64 | utm_source | |
| 340 | 8 | lead_cookie_id u64 | |
| 348 | 128 | lead_affiliate_site | |
| 476 | 4 | len(url) u32 | max 128 |
| 480 | 256 | url | |
| 736 | 8 | login u64 | |
| 744 | 160 | password blob | ศูนย์ ถ้าใช้รหัสปกติ |
| 904 | 8 | session u64 | |

* รหัสผ่านที่ยาว **320 ตัวอักษร** จะถูกตีความเป็น hex ของ blob 160 ไบต์ (รหัสที่เข้ารหัสไว้แล้ว)
* ตอบกลับ: `[160 B token][u64 session_id]` (`LOGIN_RESPONSE_SCHEMA`) — `Mt5Client::login` คืน `(token_hex, session)`
* ถ้า `code ≠ 0` → `Error::Authentication` (เช่น `code=3` = ข้อมูลล็อกอินถูกปฏิเสธ ยืนยันจากเซิร์ฟเวอร์จริง)

> **ข้อควรระวัง:** รหัสผ่านที่มี `#` ในตัวแปรสภาพแวดล้อมบางระบบ (เช่น `.env`) จะถูกตัดเป็น comment ทำให้ login ได้ `code=3`
> — เป็นสาเหตุจริงที่เจอระหว่างพัฒนา ตรวจความยาวรหัสผ่านก่อนสงสัยโปรโตคอล

## 9. Schemas

ตาราง field/offset ทั้งหมดอยู่ใน [reference/schemas.md](reference/schemas.md) (generate จาก pymt5) และใน `src/schemas.rs`
อย่าแก้ `schemas.rs` ด้วยมือ — รัน `python3 tools/gen_schemas.py <pymt5-dir> src/schemas.rs`
(อ่านซอร์ส Python ด้วย `ast` โดยไม่รันโค้ด)

## 10. ตาราง command

ดู [reference/commands.md](reference/commands.md)
