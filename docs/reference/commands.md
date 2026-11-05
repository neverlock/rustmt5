# Command reference

คอลัมน์ "ยืนยันจริง" = เคยรันกับเซิร์ฟเวอร์ build 6182 แล้วได้ผลถูกต้อง · "mock" = ทดสอบกับ mock server เท่านั้น ·
"ไม่ได้ทดสอบ" = พอร์ตตาม pymt5 แต่ยังไม่เคยยิงจริง (เช่นคำสั่งที่เปลี่ยนสถานะบัญชี)

| id | constant | ทิศทาง | Request payload | Response / push body | เมธอดหลัก | สถานะ |
|---|---|---|---|---|---|---|
| 0 | `CMD_BOOTSTRAP` | req/resp | token 64 B | `build u16 ‖ token 64 ‖ key` | `Transport::connect` | ยืนยันจริง |
| 2 | `CMD_LOGOUT` | req/resp | – | – | `logout` | ยืนยันจริง (ใช้ตอน `close`) |
| 3 | `CMD_GET_ACCOUNT` | req/resp | – | account response (ดู [account-and-push](../account-and-push.md)) | `get_account` | ยืนยันจริง |
| 4 | `CMD_GET_POSITIONS_ORDERS` | req/resp **และ push** | – | `[n pos][pos]*[n ord][ord]*` | `get_positions_and_orders`, `on_position_update`… | ยืนยันจริง |
| 5 | `CMD_GET_TRADE_HISTORY` | req/resp | `from i32 ‖ to i32` (วินาที, 0=ไม่จำกัด) | `[n deal][deal]*[n ord][ord]*` | `get_trade_history` | ยืนยันจริง |
| 6 | `CMD_GET_SYMBOLS` | req/resp | – | `[n][SYMBOL_BASIC]*` | `get_symbols(false)` | ไม่ได้ทดสอบ (ใช้ 34) |
| 7 | `CMD_SUBSCRIBE_TICKS` | req/resp | `[n][id]*` (ชุดเต็มที่ต้องการ) | – | `subscribe_ticks` | ยืนยันจริง |
| 8 | `CMD_TICK_PUSH` | push | – | `TICK_SCHEMA` ต่อกันหลายรายการ | `on_tick`, `on_tick_event` | ยืนยันจริง |
| 9 | `CMD_GET_SYMBOL_GROUPS` | req/resp | – | `[n][group 256 B]*` | `get_symbol_groups` | ยืนยันจริง |
| 10 | `CMD_TRADE_UPDATE_PUSH` | push | – | `update_type u32 …` | `on_trade_transaction` | mock/ไม่ได้ทดสอบ |
| 11 | `CMD_GET_RATES` | req/resp | `symbol(FS64) ‖ period u16 ‖ from i32 ‖ to i32` | `RATE_BAR` ต่อกัน | `get_rates` | ยืนยันจริง |
| 12 | `CMD_TRADE_REQUEST` | req/resp | `TRADE_REQUEST_SCHEMA` | `TRADE_RESPONSE_SCHEMA` | `trade_request`, `buy_market`… | mock เท่านั้น (**ไม่เคยส่งออเดอร์จริง**) |
| 13 | `CMD_SYMBOL_UPDATE_PUSH` | push | – | (schema ไม่แน่นอน) ส่ง raw | `on_symbol_update` | ไม่ได้ทดสอบ |
| 14 | `CMD_ACCOUNT_UPDATE_PUSH` | push | – | เหมือน cmd 3 | `on_account_update` | ไม่ได้ทดสอบ |
| 15 | `CMD_LOGIN_STATUS_PUSH` | push | – | raw | `on_login_status` | ไม่ได้ทดสอบ |
| 17 | `CMD_SYMBOL_DETAILS_PUSH` | push | – | `SYMBOL_DETAILS` ต่อกัน | `on_symbol_details` | ไม่ได้ทดสอบ |
| 18 | `CMD_GET_FULL_SYMBOLS` | req/resp | **`[n][symbol_id]*`** (ไม่ใช่ชื่อ!) | `[n][FULL_SYMBOL]*` | `get_full_symbols`, `get_full_symbol_info` | ยืนยันจริง |
| 19 | `CMD_TRADE_RESULT_PUSH` | push | – | `TRADE_RESULT_PUSH ‖ TRADE_RESULT_RESPONSE` | `on_trade_result` | mock/ไม่ได้ทดสอบ |
| 20 | `CMD_GET_SPREADS` | req/resp | `[n][id]*` หรือว่าง | `[n][SPREAD]*` | `get_spreads` | ไม่ได้ทดสอบ |
| 22 | `CMD_SUBSCRIBE_BOOK` | req/resp | `[n][id]*` | – | `subscribe_book` | ไม่ได้ทดสอบ |
| 23 | `CMD_BOOK_PUSH` | push | – | `[n]([BOOK_HEADER][BOOK_LEVEL]*)*` | `on_book_update` | unit test เท่านั้น |
| 24 | `CMD_CHANGE_PASSWORD` | req/resp | `is_investor u8 ‖ new FS64 ‖ old FS64` | `i32` status | `change_password` | ไม่ได้ทดสอบ (เปลี่ยนรหัสจริง!) |
| 27 | `CMD_VERIFY_CODE` | req/resp | code FS64 / payload เปิดบัญชี | – / `u8 email ‖ u8 phone` | `verify_code`, `request_opening_verification` | ไม่ได้ทดสอบ |
| 28 | `CMD_LOGIN` | req/resp | 912 B (ดู [protocol](../protocol.md)) | `token 160 ‖ session u64` | `login`, `login_with` | ยืนยันจริง |
| 29 | `CMD_INIT` | req/resp | 744 B | – | `init_session`, `initialize` | ยืนยันจริง (code 0) |
| 30 | `CMD_OPEN_DEMO` | req/resp | payload เปิดบัญชี | `OPEN_ACCOUNT_RESPONSE` | `open_demo_account` | ไม่ได้ทดสอบ |
| 34 | `CMD_GET_SYMBOLS_GZIP` | req/resp | – | `4 B ‖ zlib([n][SYMBOL_BASIC]*)` | `get_symbols(true)`, `load_symbols` | ยืนยันจริง |
| 39 | `CMD_OPEN_REAL` | req/resp | payload เปิดบัญชีจริง + เอกสาร | `OPEN_ACCOUNT_RESPONSE` | `open_real_account` | ไม่ได้ทดสอบ |
| 40 | `CMD_SEND_VERIFY_CODES` | req/resp | payload เปิดบัญชี | `u8 ‖ u8` | `submit_opening_verification` | ไม่ได้ทดสอบ |
| 41 | `CMD_TRADER_PARAMS` | req/resp | – | `FS32 ‖ FS32` | `trader_params` | ไม่ได้ทดสอบ |
| 42 | `CMD_NOTIFY` | req/resp | message FS256 | – | `send_notification` | ไม่ได้ทดสอบ |
| 43 | `CMD_OTP_SETUP` | req/resp | payload OTP | – | `enable_otp`, `disable_otp` | ไม่ได้ทดสอบ |
| 44 | `CMD_GET_CORPORATE_LINKS` | req/resp | – | `[n][CORPORATE_LINK]*` | `get_corporate_links` | ไม่ได้ทดสอบ |
| 51 | `CMD_PING` | req/resp | – | – | `ping` | ยืนยันจริง (~106 ms) |
| 52 | (ไม่มีชื่อ) | req/resp | – | – | `send_bootstrap_command_52` | ไม่ได้ทดสอบ |

id อื่นใน `VALID_COMMANDS` (21, 25, 33, 37, 50, 100–112) เป็นค่าที่ transport ยอมให้ส่งผ่าน `send_raw_command`
แต่ยังไม่รู้ความหมาย

## หมายเหตุ

* **cmd 4 / cmd 14 เป็นทั้ง response และ push** — listener ที่ลงทะเบียนไว้จะถูกเรียกทุกครั้งที่ได้ response ของ cmd นั้นด้วย
  (ไม่ใช่เฉพาะ push) เพราะ transport ไม่แยกแยะ
* `period` ของ cmd 11 ถูก map จากนาที → รหัส (`1,5,15,30` เท่าเดิม, `60→16385`, `240→16388`, `1440→16408`, `10080→32769`, `43200→49153`) ด้วย `period_map()`
* subscribe ticks/book: เซิร์ฟเวอร์ **แทนที่** ชุดเดิมทั้งชุด ดังนั้นไลบรารีจะรวม id เดิม + ใหม่แล้วส่งชุดเต็มเสมอ; ส่ง `[0]` = ยกเลิกทั้งหมด
