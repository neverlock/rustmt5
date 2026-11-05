//! WebSocket transport: bootstrap/key exchange, encryption, FIFO command dispatch
//! (port of `pymt5.transport`).

use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};

use crate::constants::{CMD_BOOTSTRAP, DEFAULT_COMMAND_TIMEOUT, DEFAULT_TOKEN_LENGTH, VALID_COMMANDS};
use crate::crypto::{initial_cipher, AesCipher};
use crate::error::{Error, Result};
use crate::metrics::MetricsCollector;
use crate::protocol::{build_command, pack_outer, parse_response_frame, unpack_outer};
use crate::rate_limiter::TokenBucketRateLimiter;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<Ws, Message>;
type WsStream = SplitStream<Ws>;

/// Connection lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    Disconnected,
    Connecting,
    Ready,
    Closing,
    Error,
}

/// A decoded server response / push.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub command: u16,
    pub code: u8,
    pub body: Vec<u8>,
}

/// Push/response listener.
pub type Listener = Arc<dyn Fn(&CommandResult) + Send + Sync>;
/// Identifies a registered listener for [`Transport::off`].
pub type ListenerId = u64;

/// Description of a listener that panicked.
#[derive(Debug, Clone)]
pub struct CallbackError {
    pub command: u16,
    pub message: String,
}
pub type CallbackErrorHandler = Arc<dyn Fn(&CallbackError) + Send + Sync>;

#[derive(Clone)]
enum Failure {
    Closed,
    Disconnected(String),
}

type Waiter = oneshot::Sender<std::result::Result<CommandResult, Failure>>;

struct Inner {
    uri: String,
    timeout: Duration,
    state: Mutex<TransportState>,
    sink: AsyncMutex<Option<WsSink>>,
    cipher: RwLock<AesCipher>,
    token: Mutex<Vec<u8>>,
    pending: Mutex<HashMap<u16, VecDeque<(u64, Waiter)>>>,
    listeners: Mutex<HashMap<u16, Vec<(ListenerId, Listener)>>>,
    next_id: AtomicU64,
    rate_limiter: TokenBucketRateLimiter,
    on_disconnect: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    error_handlers: Mutex<Vec<CallbackErrorHandler>>,
    recv_task: Mutex<Option<JoinHandle<()>>>,
    shutdown: AtomicBool,
    metrics: Option<Arc<dyn MetricsCollector>>,
    ca_bundle: Option<std::path::PathBuf>,
    proxy: Mutex<Option<String>>,
    last_message_at: Mutex<Option<Instant>>,
    server_build: AtomicU32,
}

/// Encrypted MT5 Web Terminal WebSocket transport. Cheap to clone (shared state).
#[derive(Clone)]
pub struct Transport {
    inner: Arc<Inner>,
}

impl Transport {
    pub fn new(uri: impl Into<String>) -> Self { Self::with_options(uri, DEFAULT_COMMAND_TIMEOUT, 0.0, 20, None, None) }

    pub fn with_options(
        uri: impl Into<String>,
        timeout_secs: f64,
        rate_limit: f64,
        rate_burst: u32,
        metrics: Option<Arc<dyn MetricsCollector>>,
        ca_bundle: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                uri: uri.into(),
                timeout: Duration::from_secs_f64(timeout_secs),
                state: Mutex::new(TransportState::Disconnected),
                sink: AsyncMutex::new(None),
                cipher: RwLock::new(initial_cipher()),
                token: Mutex::new(vec![0; DEFAULT_TOKEN_LENGTH]),
                pending: Mutex::new(HashMap::new()),
                listeners: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                rate_limiter: TokenBucketRateLimiter::new(rate_limit, rate_burst),
                on_disconnect: Mutex::new(None),
                error_handlers: Mutex::new(Vec::new()),
                recv_task: Mutex::new(None),
                shutdown: AtomicBool::new(false),
                metrics,
                ca_bundle,
                proxy: Mutex::new(None),
                last_message_at: Mutex::new(None),
                server_build: AtomicU32::new(0),
            }),
        }
    }

    pub fn uri(&self) -> &str { &self.inner.uri }
    pub fn state(&self) -> TransportState { *self.inner.state.lock().unwrap() }
    pub fn is_ready(&self) -> bool { self.state() == TransportState::Ready }
    /// Server build number from the bootstrap response prefix (0 if unknown).
    pub fn server_build(&self) -> u32 { self.inner.server_build.load(Ordering::Relaxed) }
    /// Time of the last message received from the server.
    pub fn last_message_at(&self) -> Option<Instant> { *self.inner.last_message_at.lock().unwrap() }

    fn set_state(&self, s: TransportState) { *self.inner.state.lock().unwrap() = s; }

    /// Register the callback invoked when the socket drops unexpectedly.
    pub fn set_on_disconnect(&self, cb: Option<Arc<dyn Fn() + Send + Sync>>) { *self.inner.on_disconnect.lock().unwrap() = cb; }

    pub fn add_callback_error_handler(&self, h: CallbackErrorHandler) { self.inner.error_handlers.lock().unwrap().push(h); }

    /// Register a listener for `command`. Listeners run on the receive task: keep them short.
    pub fn on(&self, command: u16, cb: impl Fn(&CommandResult) + Send + Sync + 'static) -> ListenerId {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.listeners.lock().unwrap().entry(command).or_default().push((id, Arc::new(cb)));
        id
    }

    /// Remove one listener, or all listeners of `command` when `id` is `None`.
    pub fn off(&self, command: u16, id: Option<ListenerId>) {
        let mut l = self.inner.listeners.lock().unwrap();
        match id {
            None => { l.remove(&command); }
            Some(id) => { if let Some(v) = l.get_mut(&command) { v.retain(|(i, _)| *i != id); } }
        }
    }

    /// Open the socket, run the bootstrap handshake and switch to the session key.
    /// Route the WebSocket through an HTTP `CONNECT` proxy (`http://[user:pass@]host:port`); `None` = direct.
    pub fn set_proxy(&self, proxy: Option<String>) { *self.inner.proxy.lock().unwrap() = proxy; }

    pub async fn connect(&self) -> Result<()> {
        if self.inner.sink.lock().await.is_some() {
            self.close().await;
        }
        // rustls needs a process-level crypto provider; ignore "already installed".
        let _ = rustls::crypto::ring::default_provider().install_default();
        self.set_state(TransportState::Connecting);
        self.inner.shutdown.store(false, Ordering::SeqCst);
        *self.inner.cipher.write().unwrap() = initial_cipher();
        *self.inner.token.lock().unwrap() = vec![0; DEFAULT_TOKEN_LENGTH];
        info!("connecting to {}", self.inner.uri);

        let res = self.connect_inner().await;
        if res.is_err() {
            self.teardown().await;
            self.set_state(TransportState::Error);
        }
        res
    }

    /// rustls config: built-in webpki roots plus any extra CA bundle (explicit path or the
    /// standard `SSL_CERT_FILE`). Certificates are only ever *added*; verification stays on.
    fn tls_connector(&self) -> Result<Connector> {
        let mut roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let extra = self.inner.ca_bundle.clone().or_else(|| std::env::var_os("SSL_CERT_FILE").map(Into::into));
        if let Some(path) = extra {
            use rustls::pki_types::{pem::PemObject, CertificateDer};
            match CertificateDer::pem_file_iter(&path) {
                Ok(iter) => {
                    let (mut ok, mut bad) = (0, 0);
                    for cert in iter.flatten() {
                        if roots.add(cert).is_ok() { ok += 1 } else { bad += 1 }
                    }
                    debug!("loaded {ok} extra CA certs from {} ({bad} rejected)", path.display());
                }
                Err(e) => warn!("could not read CA bundle {}: {e}", path.display()),
            }
        }
        let cfg = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        Ok(Connector::Rustls(Arc::new(cfg)))
    }

    async fn connect_inner(&self) -> Result<()> {
        let uri = &self.inner.uri;
        let mut req = uri.as_str().into_client_request()?;
        req.headers_mut().insert("Origin", "https://web.metatrader.app".parse().unwrap());
        let cfg = WebSocketConfig::default().max_message_size(None).max_frame_size(None);
        let proxy = self.inner.proxy.lock().unwrap().clone();
        let connector = self.tls_connector().map_err(|e| e.to_string());
        let connecting = async {
            let connector = connector?;
            match proxy {
                None => tokio_tungstenite::connect_async_tls_with_config(req, Some(cfg), false, Some(connector)).await.map_err(|e| e.to_string()),
                Some(p) => {
                    let (host, port) = {
                        let u = req.uri();
                        (u.host().unwrap_or_default().to_string(), u.port_u16().unwrap_or(if u.scheme_str() == Some("ws") { 80 } else { 443 }))
                    };
                    let stream = http_connect(&p, &host, port).await.map_err(|e| format!("proxy: {e}"))?;
                    tokio_tungstenite::client_async_tls_with_config(req, stream, Some(cfg), Some(connector)).await.map_err(|e| e.to_string())
                }
            }
        };
        let (ws, _) = tokio::time::timeout(self.inner.timeout, connecting)
            .await
            .map_err(|_| Error::Connection { message: format!("connect timed out after {:?}", self.inner.timeout), server_uri: uri.clone() })?
            .map_err(|message| Error::Connection { message, server_uri: uri.clone() })?;
        let (sink, stream) = ws.split();
        *self.inner.sink.lock().await = Some(sink);
        let this = self.clone();
        *self.inner.recv_task.lock().unwrap() = Some(tokio::spawn(async move { this.recv_loop(stream).await }));
        debug!("websocket open, sending bootstrap");

        let token = self.inner.token.lock().unwrap().clone();
        let bootstrap = self.send_raw(CMD_BOOTSTRAP, &token, false).await?;
        if bootstrap.code != 0 {
            return Err(Error::connection(format!("bootstrap failed: code={}", bootstrap.code)));
        }
        if bootstrap.body.len() < 66 {
            return Err(Error::connection(format!("bootstrap response too short: {}", bootstrap.body.len())));
        }
        *self.inner.token.lock().unwrap() = bootstrap.body[2..66].to_vec();
        *self.inner.cipher.write().unwrap() = AesCipher::new(&bootstrap.body[66..])?;
        self.inner.server_build.store(u16::from_le_bytes([bootstrap.body[0], bootstrap.body[1]]) as u32, Ordering::Relaxed);
        self.set_state(TransportState::Ready);
        if let Some(m) = &self.inner.metrics { m.on_connect(); }
        info!("transport ready (key exchanged)");
        Ok(())
    }

    async fn teardown(&self) {
        let task = self.inner.recv_task.lock().unwrap().take();
        if let Some(t) = task {
            t.abort();
            let _ = t.await;
        }
        if let Some(mut sink) = self.inner.sink.lock().await.take() {
            let _ = sink.close().await;
        }
    }

    /// Close the socket and fail all in-flight commands.
    pub async fn close(&self) {
        info!("closing transport");
        self.set_state(TransportState::Closing);
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.teardown().await;
        self.fail_all(Failure::Closed);
        self.set_state(TransportState::Disconnected);
        debug!("transport closed");
    }

    /// Send a command and wait for its response (requires a ready transport).
    pub async fn send_command(&self, command: u16, payload: &[u8]) -> Result<CommandResult> {
        self.send_raw(command, payload, true).await
    }

    async fn send_raw(&self, command: u16, payload: &[u8], check_ready: bool) -> Result<CommandResult> {
        if !VALID_COMMANDS.contains(&command) {
            return Err(Error::protocol(format!("unsupported command: {command}")));
        }
        if check_ready && !self.is_ready() {
            return Err(Error::session(format!("transport not ready for command {command}")));
        }
        self.inner.rate_limiter.acquire().await;

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.inner.sink.lock().await; // also serialises sends
            let sink = guard.as_mut().ok_or_else(|| Error::connection("websocket not connected"))?;
            self.inner.pending.lock().unwrap().entry(command).or_default().push_back((id, tx));
            let encrypted = self.inner.cipher.read().unwrap().encrypt(&build_command(command, payload));
            debug!("send cmd={} payload={} bytes", command, payload.len());
            if let Err(e) = sink.send(Message::Binary(pack_outer(&encrypted).into())).await {
                self.remove_pending(command, id);
                return Err(e.into());
            }
            if let Some(m) = &self.inner.metrics { m.on_command_sent(command); }
        }
        match tokio::time::timeout(self.inner.timeout, rx).await {
            Ok(Ok(Ok(res))) => Ok(res),
            Ok(Ok(Err(Failure::Closed))) => Err(Error::session("transport closed")),
            Ok(Ok(Err(Failure::Disconnected(m)))) => Err(Error::connection(m)),
            Ok(Err(_)) => Err(Error::session("transport closed")),
            Err(_) => {
                self.remove_pending(command, id);
                Err(Error::Timeout(format!("command {command} timed out after {}s", self.inner.timeout.as_secs_f64())))
            }
        }
    }

    fn remove_pending(&self, command: u16, id: u64) {
        if let Some(q) = self.inner.pending.lock().unwrap().get_mut(&command) {
            q.retain(|(i, _)| *i != id);
        }
    }

    async fn recv_loop(self, mut stream: WsStream) {
        let mut reason = String::from("connection closed by server");
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if let Err(e) = self.handle_frame(&data) {
                        error!("recv_loop parse error: {e}");
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(e) => { reason = e.to_string(); break; }
            }
        }
        if self.inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        error!("recv_loop disconnected: {reason}");
        self.fail_all(Failure::Disconnected(reason.clone()));
        self.set_state(TransportState::Error);
        if let Some(m) = &self.inner.metrics { m.on_disconnect(&reason); }
        let cb = self.inner.on_disconnect.lock().unwrap().clone();
        if let Some(cb) = cb { cb(); }
    }

    fn handle_frame(&self, raw: &[u8]) -> Result<()> {
        let (_, _, enc) = unpack_outer(raw)?;
        let dec = self.inner.cipher.read().unwrap().decrypt(enc)?;
        let frame = parse_response_frame(&dec)?;
        *self.inner.last_message_at.lock().unwrap() = Some(Instant::now());
        debug!("recv cmd={} code={} body={} bytes", frame.command, frame.code, frame.body.len());
        self.dispatch(CommandResult { command: frame.command, code: frame.code, body: frame.body });
        Ok(())
    }

    fn dispatch(&self, result: CommandResult) {
        if let Some(m) = &self.inner.metrics { m.on_command_received(result.command, result.code); }
        // resolve the oldest live waiter (FIFO)
        let mut waiter = None;
        {
            let mut p = self.inner.pending.lock().unwrap();
            if let Some(q) = p.get_mut(&result.command) {
                while let Some((_, tx)) = q.pop_front() {
                    if !tx.is_closed() { waiter = Some(tx); break; }
                }
            }
        }
        if let Some(tx) = waiter { let _ = tx.send(Ok(result.clone())); }

        let listeners: Vec<Listener> = self
            .inner
            .listeners
            .lock()
            .unwrap()
            .get(&result.command)
            .map(|v| v.iter().map(|(_, l)| l.clone()).collect())
            .unwrap_or_default();
        for cb in listeners {
            if let Err(p) = catch_unwind(AssertUnwindSafe(|| cb(&result))) {
                let message = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "callback panicked".into());
                error!("callback for cmd={} panicked: {message}", result.command);
                let ce = CallbackError { command: result.command, message };
                let handlers = self.inner.error_handlers.lock().unwrap().clone();
                for h in handlers {
                    if catch_unwind(AssertUnwindSafe(|| h(&ce))).is_err() {
                        warn!("callback error handler itself panicked");
                    }
                }
            }
        }
    }

    fn fail_all(&self, why: Failure) {
        let mut p = self.inner.pending.lock().unwrap();
        for q in p.values_mut() {
            while let Some((_, tx)) = q.pop_front() {
                let _ = tx.send(Err(why.clone()));
            }
        }
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() { out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char); } else { out.push('='); }
        }
    }
    out
}

/// Open a TCP tunnel to `host:port` through an HTTP proxy (`CONNECT`). Returns the raw stream after the `200` reply.
async fn http_connect(proxy: &str, host: &str, port: u16) -> std::result::Result<TcpStream, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = proxy.strip_prefix("http://").unwrap_or(proxy).trim_end_matches('/');
    let (auth, addr) = match rest.rsplit_once('@') { Some((a, h)) => (Some(a), h), None => (None, rest) };
    let mut s = TcpStream::connect(addr).await.map_err(|e| format!("connect {addr}: {e}"))?;
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(a) = auth { req.push_str(&format!("Proxy-Authorization: Basic {}\r\n", base64(a.as_bytes()))); }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    while !buf.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        if s.read(&mut b).await.map_err(|e| e.to_string())? == 0 { return Err("closed during CONNECT".into()); }
        buf.push(b[0]);
        if buf.len() > 16 * 1024 { return Err("CONNECT reply too large".into()); }
    }
    let status = String::from_utf8_lossy(&buf);
    let line = status.lines().next().unwrap_or_default();
    if line.split_whitespace().nth(1) != Some("200") { return Err(format!("CONNECT refused: {line}")); }
    Ok(s)
}

#[cfg(test)]
mod proxy_tests {
    use super::*;
    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"a"), "YQ==");
    }
    #[tokio::test]
    async fn connect_tunnel_through_fake_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let t = tokio::spawn(async move {
            let (mut c, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 512];
            let n = c.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.unwrap();
            req
        });
        http_connect(&format!("http://u:p@{addr}"), "example.com", 443).await.unwrap();
        let req = t.await.unwrap();
        assert!(req.starts_with("CONNECT example.com:443 HTTP/1.1"));
        assert!(req.contains("Proxy-Authorization: Basic dTpw"));
    }
}
