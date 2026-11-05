# rustmt5 — Developer Documentation

เอกสารนี้อธิบายโครงสร้างและการทำงานภายในของ `rustmt5` (พอร์ตจาก [cloudQuant/pymt5](https://github.com/cloudQuant/pymt5))
สำหรับคนที่จะ **ใช้** ไลบรารีอย่างลึก หรือ **แก้ไข/ต่อยอด** โค้ด ตัวอย่างโค้ดและชื่อ API เป็นภาษาอังกฤษตามต้นฉบับ

> สถานะอ้างอิง: ทดสอบจริงกับ Web Terminal build **6182** (broker CXMDirect) และ mock server ในรีโป
> รายละเอียดที่ "ยืนยันกับเซิร์ฟเวอร์จริงแล้ว" จะระบุไว้ในแต่ละหัวข้อ ส่วนที่ยังไม่ได้ยืนยันจะบอกตรง ๆ

## อ่านตามลำดับนี้

| ลำดับ | เอกสาร | อ่านเมื่อ |
|---|---|---|
| 1 | [architecture.md](architecture.md) | อยากเห็นภาพรวม แผนผังโมดูล โมเดล concurrency และกฎการ lock |
| 2 | [protocol.md](protocol.md) | ต้องการรู้ wire format, handshake, การเข้ารหัส, ตาราง command |
| 3 | [transport.md](transport.md) | แก้/ดีบักการเชื่อมต่อ, timeout, listener, TLS |
| 4 | [client.md](client.md) | login, heartbeat, reconnect, cache/state ของ `Mt5Client` |
| 5 | [market-data.md](market-data.md) | symbols, tick, order book, bars, full symbol info |
| 6 | [trading.md](trading.md) | positions/orders, trade request, `order_check`, สูตร profit/margin |
| 7 | [account-and-push.md](account-and-push.md) | โครงสร้าง account response, เปิดบัญชี, push handlers ทั้งหมด |
| 8 | [types-and-errors.md](types-and-errors.md) | `Value`/`Record`, `TimeArg`, ชนิดข้อผิดพลาด, "soft error" |
| 9 | [testing.md](testing.md) | การรันเทสต์, mock server, live check, ข้อควรระวังเรื่องรหัสผ่าน |
| 10 | [contributing.md](contributing.md) | สูตรเพิ่ม command/push handler/schema, ความต่างจาก Python, quirks |
| ref | [reference/schemas.md](reference/schemas.md) | ตาราง field/offset ของทุก schema (generate อัตโนมัติ) |
| ref | [reference/commands.md](reference/commands.md) | ตาราง command id ↔ constant ↔ เมธอด ↔ payload |

API doc ระดับ rustdoc: `cargo doc --no-deps --open`

## คำศัพท์สั้น ๆ

* **cmd / command** — เลขคำสั่ง u16 ในเฟรม (เช่น 28 = login) ดู [reference/commands.md](reference/commands.md)
* **push** — เฟรมที่เซิร์ฟเวอร์ส่งมาเองโดยไม่มี request (tick, account update, trade update …)
* **Record** — map แบบเรียงลำดับ `ชื่อ field → Value` แทน dict ของ Python
* **schema** — รายการ `Field { ty, len }` ที่บอกว่า binary record หนึ่งรายการเรียงอะไรบ้าง
* **wire volume** — ปริมาณแบบจำนวนเต็ม `lots × 10^8` (0.01 lot = 1 000 000) ที่ใช้บนสาย
