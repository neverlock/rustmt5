# Transport (`src/transport.rs`)

`Transport` owns the WebSocket: connecting, the handshake, encryption, matching responses to requests and fanning pushes out to
listeners. It is `Clone` (shared `Arc<Inner>`); the recv task holds its own clone.

## 1. State machine

```
            connect()                bootstrap OK
DISCONNECTED ---------> CONNECTING ------------------> READY
     ^                      | error                      |  socket dropped / closed by server
     | close()              v                            v
     +------------------- ERROR <-------------------------+
     ^
     +---- CLOSING <---- close() (transient)
```

| State | Meaning | `send_command` |
|---|---|---|
| `Disconnected` | initial / closed | `Error::Session("transport not ready ...")` |
| `Connecting` | opening the socket + bootstrap | only the internal bootstrap may send |
| `Ready` | usable | normal |
| `Closing` | `close()` in progress | not ready |
| `Error` | dropped unexpectedly (recv task ended); `on_disconnect` was called | not ready |

## 2. `connect()` step by step

1. If a socket exists, `close()` it first (guards against double connect).
2. Install the rustls crypto provider (ring) - repeated installs are harmless.
3. State `Connecting`; clear the shutdown flag; reset the cipher to the initial key and the token to 64 zero bytes.
4. Build the request with the `Origin` header; open the WebSocket within `timeout` (failure -> `Error::Connection { message, server_uri }`).
5. Split into sink/stream, store the sink, **spawn the recv task**.
6. Send the bootstrap (cmd 0) via `send_raw(check_ready = false)`.
7. Require `code == 0` and `body.len() >= 66`; `token = body[2..66]`, `cipher = AesCipher::new(body[66..])`, `server_build = u16(body[0..2])`.
8. State `Ready`, `metrics.on_connect()`.

Any failure after step 5 tears the connection down (abort recv task, close sink) and leaves the state at `Error`.

## 3. Send path (`send_raw`)

```rust
validate cmd in VALID_COMMANDS               // else Error::Protocol("unsupported command")
if check_ready && !is_ready -> Error::Session
rate_limiter.acquire().await
let (tx, rx) = oneshot::channel();
{
  let sink = self.sink.lock().await;          // one frame at a time
  pending[cmd].push_back((id, tx));           // register BEFORE sending so a fast reply is not missed
  encrypt(build_command(cmd, payload)) -> pack_outer -> sink.send
}
timeout(self.timeout, rx).await
```

Outcomes: `Ok(CommandResult)`; `Failure::Closed` -> `Error::Session("transport closed")`; `Failure::Disconnected(msg)` -> `Error::Connection`;
timeout -> the waiter is removed from `pending` and `Error::Timeout` is returned; a failed send removes the waiter and returns `Error::WebSocket`.

### Caveat: late replies

Replies have no request id. A reply that arrives *after* its command timed out is matched to the next waiter of the same command (or
dropped when there is none). Choose a generous `timeout` and avoid overlapping identical commands after a timeout. (Same as pymt5.)

## 4. The recv task

```
while let Some(msg) = ws.next():
   Binary -> handle_frame()      // unpack_outer -> decrypt -> parse_response_frame -> dispatch
             a malformed frame is logged and skipped (the connection stays up)
   Close  -> break
   Err(e) -> reason = e; break
   other (Text/Ping/Pong) -> ignore
after the loop:
   if the shutdown flag is set (we closed it) -> end quietly
   else: fail_all(Disconnected) | state = Error | metrics.on_disconnect | call the on_disconnect callback
```

Unlike pymt5, a normal close initiated by the **server** also counts as a disconnect, so auto-reconnect works when the server hangs up.

### dispatch

1. `metrics.on_command_received(cmd, code)`.
2. Pop waiters from `pending[cmd]` until one is still alive (`!tx.is_closed()`) and send it the result.
3. Clone the listener list of `cmd` out of the lock and call each in `catch_unwind`.
4. A panicking listener is logged and passed to every error handler (`add_callback_error_handler` / `Mt5Client::on_callback_error`); a panicking handler is also contained.

## 5. Listener API

```rust
let id: ListenerId = transport.on(CMD_TICK_PUSH, |r: &CommandResult| { /* sync and short */ });
transport.off(CMD_TICK_PUSH, Some(id));   // remove one
transport.off(CMD_TICK_PUSH, None);       // remove all for the command (including the client's own cache listeners!)
```

* Listeners run **in registration order**. `Mt5Client::new` registers its cache listeners first, so user handlers see up-to-date caches.
* Listeners run on the recv task: blocking one stalls all incoming data, including responses to other commands.

## 6. Rate limiter (`rate_limiter.rs`)

Token bucket: `rate` tokens/s, up to `burst`; `rate <= 0` disables it. `acquire()` refills by elapsed time and, when empty, sleeps *outside* the lock
and retries (cancel-safe). Configure with `ClientConfig { rate_limit, rate_burst }`.

## 7. TLS

* rustls with the **ring** provider; roots = built-in `webpki-roots`.
* Extra roots can be **added** via `ClientConfig::ca_bundle` (PEM path) or the standard `SSL_CERT_FILE` variable. Verification is never disabled.
  Needed behind a proxy that re-terminates TLS (in the development sandbox: `/root/.ccr/ca-bundle.crt`; without it the error is
  `invalid peer certificate: UnknownIssuer`).
* HTTP/SOCKS proxies from environment variables (`HTTPS_PROXY`) are **not** supported: the TCP connection is opened directly. For a CONNECT proxy open
  the stream yourself and use `client_async_tls_with_config` (not implemented).

## 8. Metrics

`MetricsCollector` (all methods default to no-ops): `on_command_sent`, `on_command_received`, `on_connect`, `on_disconnect(reason)`,
`on_reconnect_attempt(n)`, `on_reconnect_success(n)`; pass `Some(Arc<dyn MetricsCollector>)` in `ClientConfig.metrics`.
