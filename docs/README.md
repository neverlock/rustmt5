# rustmt5 - Developer Documentation

Internals of `rustmt5`, a Rust port of [cloudQuant/pymt5](https://github.com/cloudQuant/pymt5) (a client for the MetaTrader 5
Web Terminal binary WebSocket protocol). The audience is people who **use** the library in depth or **modify** it.

> Reference environment: Web Terminal build **6182** (CXMDirect live and demo servers) plus the in-process mock server in `tests/`.
> Each page says what was **verified against a real server**, what is **mock-only**, and what is **untested**.

## Reading order

| # | Page | Read it when you want to |
|---|---|---|
| 1 | [architecture.md](architecture.md) | see the layers, file map, data flow, concurrency model and locking rules |
| 2 | [protocol.md](protocol.md) | understand the wire format, handshake, encryption, request/push layouts |
| 3 | [transport.md](transport.md) | debug connections, timeouts, listeners, TLS |
| 4 | [client.md](client.md) | understand login, heartbeat, reconnect, and the client's caches/live state |
| 5 | [market-data.md](market-data.md) | work with symbols, ticks, order book, bars, full symbol info |
| 6 | [trading.md](trading.md) | place/modify/close orders, `order_check`, margin and profit formulas |
| 7 | [account-and-push.md](account-and-push.md) | read the account record, open accounts, use push handlers |
| 8 | [types-and-errors.md](types-and-errors.md) | use `Value`/`Record`, `TimeArg`, `Error` |
| 9 | [testing.md](testing.md) | run tests, extend the mock server, run the live/demo checks safely |
| 10 | [contributing.md](contributing.md) | add commands/handlers/schemas, know the differences from pymt5, see open work |
| ref | [reference/schemas.md](reference/schemas.md) | look up every field/offset (generated) plus the layouts pymt5 got wrong |
| ref | [reference/commands.md](reference/commands.md) | look up command ids, payloads, methods and verification status |

API docs: `cargo doc --no-deps --open`.

## Glossary

* **cmd / command** - the u16 command number inside a frame (28 = login). See [reference/commands.md](reference/commands.md).
* **push** - a frame the server sends on its own (ticks, trade updates, ...).
* **Record** - insertion-ordered `name -> Value` map, the Rust counterpart of pymt5's dicts.
* **schema** - a list of `Field { ty, len }` describing one fixed-size binary record.
* **wire volume** - integer volume `lots x 10^8` (0.01 lot = 1 000 000) used on the wire.
* **execution mode** (`trade_exemode`) - how the broker executes market deals: 0 request, 1 instant, 2 market, 3 exchange.

## What you must know before trading with this library

1. Order results are **asynchronous**: cmd 12 only acknowledges; the real outcome arrives as cmd 19 pushes. `trade_request` waits for the final one.
2. The server's cmd 4 answer (positions) is the **login-time snapshot**; the library keeps positions/orders current from cmd 10 pushes.
3. Used **margin is computed locally** (the server does not send it).
4. Everything trading-related was verified on a **demo** account only. Start there.

See [trading.md](trading.md) for details.
