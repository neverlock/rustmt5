# Command reference

Status column: **live** = exercised against a real server (build 6182; trading commands on a *demo* account) with correct results, **mock** = mock server/unit tests only, **untested** = ported from pymt5 but never sent.

| id | constant | direction | request payload | response / push body | main methods | status |
|---|---|---|---|---|---|---|
| 0 | `CMD_BOOTSTRAP` | req/resp | 64-byte token | `build u16 | token 64 | key` | `Transport::connect` | live |
| 2 | `CMD_LOGOUT` | req/resp | - | - | `logout` | live (on `close`) |
| 3 | `CMD_GET_ACCOUNT` | req/resp | - | account record ([account-and-push](../account-and-push.md)) | `get_account` | live |
| 4 | `CMD_GET_POSITIONS_ORDERS` | req/resp (+ push) | - | `[n][POSITION][n][ORDER] + 12 B trailer` - **login-time snapshot** | `get_positions_and_orders` (live state) | live |
| 5 | `CMD_GET_TRADE_HISTORY` | req/resp | `from i32 | to i32` (seconds, 0 = unbounded) | `[n][DEAL][n][ORDER]` | `get_trade_history` | live |
| 6 | `CMD_GET_SYMBOLS` | req/resp | - | `[n][SYMBOL_BASIC]` | `get_symbols(false)` | untested (34 is used) |
| 7 | `CMD_SUBSCRIBE_TICKS` | req/resp | `[n][id]*` (the full desired set) | - | `subscribe_ticks` | live |
| 8 | `CMD_TICK_PUSH` | push | - | `TICK_SCHEMA` records back to back | `on_tick`, `on_tick_event` | live |
| 9 | `CMD_GET_SYMBOL_GROUPS` | req/resp | - | `[n][group 256 B]` | `get_symbol_groups` | live |
| 10 | `CMD_TRADE_UPDATE_PUSH` | push | - | type 2: deal+position+balance (760 B); type 0/1: order (368 B) | `on_trade_transaction`, live state | live |
| 11 | `CMD_GET_RATES` | req/resp | `symbol(64) | period u16 | from i32 | to i32` | bars | `get_rates` | live |
| 12 | `CMD_TRADE_REQUEST` | req/resp | **248-byte request** | 4-byte ack (result comes via cmd 19) | `trade_request`, helpers, `order_send` | live (demo) |
| 13 | `CMD_SYMBOL_UPDATE_PUSH` | push | - | raw (schema unknown) | `on_symbol_update` | untested |
| 14 | `CMD_ACCOUNT_UPDATE_PUSH` | push | - | same as cmd 3 | `on_account_update` | untested (no push seen) |
| 15 | `CMD_LOGIN_STATUS_PUSH` | push | - | 4 bytes observed right after login | `on_login_status` | live (observed) |
| 17 | `CMD_SYMBOL_DETAILS_PUSH` | push | - | `SYMBOL_DETAILS` records | `on_symbol_details` | untested |
| 18 | `CMD_GET_FULL_SYMBOLS` | req/resp | **`[n][symbol_id]*`** | `[n][FULL_SYMBOL]` | `get_full_symbols`, `get_full_symbol_info` | live |
| 19 | `CMD_TRADE_RESULT_PUSH` | push | - | `token | request 248 | result 128`, several per request | `on_trade_result`, `trade_request` | live (demo) |
| 20 | `CMD_GET_SPREADS` | req/resp | `[n][id]*` or empty | `[n][SPREAD]` | `get_spreads` | untested |
| 22 | `CMD_SUBSCRIBE_BOOK` | req/resp | `[n][id]*` | - | `subscribe_book` | untested |
| 23 | `CMD_BOOK_PUSH` | push | - | `[n]([BOOK_HEADER][BOOK_LEVEL]*)*` | `on_book_update` | mock |
| 24 | `CMD_CHANGE_PASSWORD` | req/resp | `is_investor u8 | new 64 B | old 64 B` | `i32` status | `change_password` | untested (changes the password!) |
| 27 | `CMD_VERIFY_CODE` | req/resp | code / opening payload | - / `u8 email | u8 phone` | `verify_code`, `request_opening_verification` | untested |
| 28 | `CMD_LOGIN` | req/resp | 912 B ([protocol](../protocol.md)) | `token 160 | session u64` | `login`, `login_with` | live |
| 29 | `CMD_INIT` | req/resp | 744 B | - | `init_session`, `initialize` | live (code 0) |
| 30 | `CMD_OPEN_DEMO` | req/resp | opening payload | `OPEN_ACCOUNT_RESPONSE` | `open_demo_account` | untested |
| 34 | `CMD_GET_SYMBOLS_GZIP` | req/resp | - | `4 B | zlib([n][SYMBOL_BASIC])` | `get_symbols(true)`, `load_symbols` | live |
| 39 | `CMD_OPEN_REAL` | req/resp | real-account payload + documents | `OPEN_ACCOUNT_RESPONSE` | `open_real_account` | untested |
| 40 | `CMD_SEND_VERIFY_CODES` | req/resp | opening payload | `u8 | u8` | `submit_opening_verification` | untested |
| 41 | `CMD_TRADER_PARAMS` | req/resp | - | `FS32 | FS32` | `trader_params` | untested |
| 42 | `CMD_NOTIFY` | req/resp | message (256 B) | - | `send_notification` | untested |
| 43 | `CMD_OTP_SETUP` | req/resp | OTP payload | - | `enable_otp`, `disable_otp` | untested |
| 44 | `CMD_GET_CORPORATE_LINKS` | req/resp | - | `[n][CORPORATE_LINK]` | `get_corporate_links` | untested |
| 51 | `CMD_PING` | req/resp | - | - | `ping` | live (~106 ms) |
| 52 | (unnamed) | req/resp | - | - | `send_bootstrap_command_52` | untested |

Other ids in `VALID_COMMANDS` (21, 25, 33, 37, 50, 100-112) are accepted by the transport (use `send_raw_command`) but their meaning is unknown.

## Notes

* **cmd 4 and cmd 14 are both responses and pushes** - listeners fire for responses too, the transport cannot tell them apart.
* Bar `period` is minutes mapped to protocol codes (`period_map()`): 60 -> 16385, 240 -> 16388, 1440 -> 16408, 10080 -> 32769, 43200 -> 49153.
* Subscribe commands **replace** the server-side set; the client always sends the merged full set (`[0]` clears).
* A non-zero response `code` is an error and the body is usually empty (login `3`, cmd 18 with the old request `1`).
