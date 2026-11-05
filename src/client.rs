//! High level async client (port of `pymt5.client`). Feature areas live in sibling modules
//! (`account`, `market_data`, `currency`, `trading`, `order_helpers`, `push_handlers`) as
//! additional `impl Mt5Client` blocks, mirroring the Python mixins.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use rand::Rng;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::constants::*;
use crate::error::{Error, Result};
use crate::helpers::{build_client_id, bytes_to_hex, hex_to_bytes};
use crate::metrics::MetricsCollector;
use crate::protocol::{Record, SeriesCodec, SerField};
use crate::schemas::LOGIN_RESPONSE_SCHEMA;
use crate::transport::{CommandResult, Transport};
use crate::types::{HealthStatus, SymbolInfo};

pub const MT5_TERMINAL_VERSION: i64 = 500;

/// Publicly observed Web Terminal builds -> release date.
pub fn observed_build_release_date(build: i64) -> &'static str {
    match build {
        5687 => "15 Mar 2026",
        _ => "",
    }
}

/// Client construction options (mirrors the `MT5WebClient.__init__` keyword arguments).
#[derive(Clone)]
pub struct ClientConfig {
    pub uri: String,
    /// Per-command timeout in seconds.
    pub timeout: f64,
    pub heartbeat_interval: f64,
    pub tick_history_limit: usize,
    pub max_tick_symbols: usize,
    pub auto_reconnect: bool,
    pub max_reconnect_attempts: u32,
    pub reconnect_delay: f64,
    pub max_reconnect_delay: f64,
    /// Commands per second (0 disables rate limiting).
    pub rate_limit: f64,
    pub rate_burst: u32,
    pub metrics: Option<Arc<dyn MetricsCollector>>,
    /// Symbol cache TTL in seconds (0 disables caching in `load_symbols`).
    pub symbol_cache_ttl: f64,
    /// Extra PEM root CA bundle to trust in addition to the built-in roots
    /// (defaults to the `SSL_CERT_FILE` environment variable when unset).
    pub ca_bundle: Option<std::path::PathBuf>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            uri: DEFAULT_WS_URI.to_string(),
            timeout: DEFAULT_COMMAND_TIMEOUT,
            heartbeat_interval: 30.0,
            tick_history_limit: 10_000,
            max_tick_symbols: 0,
            auto_reconnect: false,
            max_reconnect_attempts: 5,
            reconnect_delay: 3.0,
            max_reconnect_delay: 60.0,
            rate_limit: 0.0,
            rate_burst: 20,
            metrics: None,
            symbol_cache_ttl: 0.0,
            ca_bundle: None,
        }
    }
}

impl ClientConfig {
    pub fn new(uri: impl Into<String>) -> Self { Self { uri: uri.into(), ..Default::default() } }
}

/// Login parameters (see [`Mt5Client::login_with`]).
#[derive(Clone, Default)]
pub struct LoginParams {
    pub login: u64,
    /// Plain password, or a 320-char hex blob for pre-encrypted passwords.
    pub password: String,
    pub url: String,
    pub session: u64,
    pub otp: String,
    pub version: u32,
    pub cid: Option<Vec<u8>>,
    pub lead_cookie_id: u64,
    pub lead_affiliate_site: String,
    pub utm_campaign: String,
    pub utm_source: String,
    pub auto_heartbeat: bool,
}

impl LoginParams {
    pub fn new(login: u64, password: impl Into<String>) -> Self {
        Self { login, password: password.into(), auto_heartbeat: true, ..Default::default() }
    }
}

impl std::fmt::Debug for LoginParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginParams").field("login", &self.login).field("password", &"<redacted>").finish_non_exhaustive()
    }
}

pub(crate) type DisconnectCb = Arc<dyn Fn() + Send + Sync>;
pub(crate) type HealthCb = Arc<dyn Fn(&HealthStatus) + Send + Sync>;

/// Mutable client state shared with push handlers.
#[derive(Default)]
pub(crate) struct State {
    pub symbols: HashMap<String, SymbolInfo>,
    pub symbols_by_id: HashMap<u32, SymbolInfo>,
    pub full_symbols: HashMap<String, Record>,
    pub symbols_loaded_at: Option<Instant>,
    pub tick_cache_by_id: HashMap<u32, Record>,
    pub tick_cache_by_name: HashMap<String, Record>,
    pub tick_history_by_id: HashMap<u32, VecDeque<Record>>,
    pub tick_history_access_order: Vec<u32>,
    pub book_cache_by_id: HashMap<u32, Record>,
    pub book_cache_by_name: HashMap<String, Record>,
    pub last_error: (i64, String),
    pub logged_in: bool,
    pub bootstrap_pristine: bool,
    pub closing: bool,
    pub login_params: Option<LoginParams>,
    pub subscribed_ids: Vec<u32>,
    pub subscribed_book_ids: Vec<u32>,
    pub reconnect_count: u32,
    pub connected_at: Option<Instant>,
    pub health_threshold_ms: f64,
}

pub(crate) struct Inner {
    pub config: ClientConfig,
    pub transport: Transport,
    pub state: Arc<Mutex<State>>,
    heartbeat: Mutex<Option<JoinHandle<()>>>,
    reconnect: Mutex<Option<JoinHandle<()>>>,
    on_disconnect: Mutex<Option<DisconnectCb>>,
    health_callbacks: Mutex<Vec<HealthCb>>,
}

/// Async MT5 Web Terminal client. Cheap to clone; all clones share one connection.
#[derive(Clone)]
pub struct Mt5Client {
    pub(crate) inner: Arc<Inner>,
}

/// Errors the Python library swallows in "best effort" helpers (`ValueError`/`RuntimeError`/`KeyError`).
pub(crate) fn is_soft_error(e: &Error) -> bool {
    matches!(e, Error::Protocol(_) | Error::Validation(_) | Error::Session(_) | Error::Trade { .. } | Error::SymbolNotFound(_))
}

/// Truncate to `n` characters (Python `s[:n]`).
pub(crate) fn trunc(s: &str, n: usize) -> String { s.chars().take(n).collect() }

impl Mt5Client {
    pub fn new(config: ClientConfig) -> Self {
        let transport = Transport::with_options(config.uri.clone(), config.timeout, config.rate_limit, config.rate_burst, config.metrics.clone(), config.ca_bundle.clone());
        let state = Arc::new(Mutex::new(State { health_threshold_ms: 5000.0, ..Default::default() }));
        let client = Self {
            inner: Arc::new(Inner {
                config,
                transport,
                state,
                heartbeat: Mutex::new(None),
                reconnect: Mutex::new(None),
                on_disconnect: Mutex::new(None),
                health_callbacks: Mutex::new(Vec::new()),
            }),
        };
        client.install_cache_listeners();
        let weak = Arc::downgrade(&client.inner);
        client.inner.transport.set_on_disconnect(Some(Arc::new(move || {
            if let Some(inner) = weak.upgrade() { Mt5Client { inner }.handle_disconnect(); }
        })));
        client
    }

    /// Client with default configuration for `uri`.
    pub fn with_uri(uri: impl Into<String>) -> Self { Self::new(ClientConfig::new(uri)) }

    pub fn config(&self) -> &ClientConfig { &self.inner.config }
    pub fn transport(&self) -> &Transport { &self.inner.transport }
    pub(crate) fn st(&self) -> MutexGuard<'_, State> { self.inner.state.lock().unwrap_or_else(|e| e.into_inner()) }

    pub fn is_connected(&self) -> bool { self.inner.transport.is_ready() && self.st().logged_in }
    pub fn server_build(&self) -> u32 { self.inner.transport.server_build() }

    /// Register a callback fired when the socket drops unexpectedly.
    pub fn on_disconnect(&self, cb: impl Fn() + Send + Sync + 'static) { *self.inner.on_disconnect.lock().unwrap() = Some(Arc::new(cb)); }

    pub async fn connect(&self) -> Result<&Self> {
        self.inner.transport.connect().await?;
        {
            let mut st = self.st();
            st.bootstrap_pristine = true;
            st.connected_at = Some(Instant::now());
        }
        match self.inner.transport.server_build() {
            0 => info!("connected to {}", self.inner.config.uri),
            b => info!("connected to {} (server_build={})", self.inner.config.uri, b),
        }
        Ok(self)
    }

    /// Official-style alias for cmd=29 session initialisation (connects first if needed).
    pub async fn initialize(&self, version: u32, password: &str, otp: &str, cid: Option<&[u8]>) -> Result<CommandResult> {
        if !self.inner.transport.is_ready() { self.connect().await?; }
        self.init_session(version, password, otp, cid).await
    }

    pub async fn close(&self) {
        self.st().closing = true;
        self.stop_heartbeat();
        if let Some(h) = self.inner.reconnect.lock().unwrap().take() { h.abort(); }
        let logged_in = self.st().logged_in;
        if logged_in {
            if let Err(e) = self.logout().await { debug!("logout during close() failed: {e}"); }
            self.st().logged_in = false;
        }
        self.inner.transport.close().await;
        {
            let mut st = self.st();
            st.bootstrap_pristine = false;
            st.closing = false;
            st.login_params = None; // drop stored credentials
        }
        info!("connection closed");
    }

    /// Official-style alias of [`Mt5Client::close`].
    pub async fn shutdown(&self) { self.close().await }

    /// Latest compatibility-layer error `(code, message)`.
    pub fn last_error(&self) -> (i64, String) { self.st().last_error.clone() }
    pub(crate) fn clear_last_error(&self) { self.st().last_error = (0, String::new()); }
    pub(crate) fn fail_last_error(&self, code: i64, message: impl Into<String>) {
        let message = message.into();
        debug!("compat helper error {code}: {message}");
        self.st().last_error = (code, message);
    }

    // ---- heartbeat ----

    fn start_heartbeat(&self) {
        let mut slot = self.inner.heartbeat.lock().unwrap();
        if slot.is_some() { return; }
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let interval = Duration::from_secs_f64(self.inner.config.heartbeat_interval);
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(inner) = weak.upgrade() else { return };
                match (Mt5Client { inner }).ping().await {
                    Ok(()) => debug!("heartbeat ping ok"),
                    Err(e) => warn!("heartbeat ping failed: {e}"),
                }
            }
        }));
    }

    fn stop_heartbeat(&self) {
        if let Some(h) = self.inner.heartbeat.lock().unwrap().take() { h.abort(); }
    }

    // ---- reconnect ----

    fn handle_disconnect(&self) {
        {
            let mut st = self.st();
            st.logged_in = false;
            st.bootstrap_pristine = false;
        }
        // Called from the receive task, which the heartbeat does not share: safe to abort.
        self.stop_heartbeat();
        warn!("disconnected from server");
        if let Some(cb) = self.inner.on_disconnect.lock().unwrap().clone() { cb(); }
        let (closing, has_creds) = { let st = self.st(); (st.closing, st.login_params.is_some()) };
        if self.inner.config.auto_reconnect && !closing && has_creds {
            let mut slot = self.inner.reconnect.lock().unwrap();
            if slot.as_ref().is_some_and(|h| !h.is_finished()) {
                debug!("reconnect already in progress, skipping");
                return;
            }
            let this = self.clone();
            *slot = Some(tokio::spawn(async move { this.reconnect_loop().await }));
        }
    }

    /// Reconnect with exponential backoff + jitter:
    /// `min(base * 2^(attempt-1) + rand(0, base), max)`.
    ///
    /// Unlike the Python original the same [`Transport`] is reused, so listeners
    /// (including the tick/book caches) survive reconnects.
    async fn reconnect_loop(&self) {
        let cfg = &self.inner.config;
        for attempt in 1..=cfg.max_reconnect_attempts {
            let jitter = rand::rng().random_range(0.0..=cfg.reconnect_delay.max(0.0));
            let delay = (cfg.reconnect_delay * 2f64.powi(attempt as i32 - 1) + jitter).min(cfg.max_reconnect_delay);
            info!("reconnect attempt {attempt}/{} (delay={delay:.1}s)", cfg.max_reconnect_attempts);
            if let Some(m) = &cfg.metrics { m.on_reconnect_attempt(attempt); }
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            let attempt_result: Result<()> = async {
                self.inner.transport.close().await;
                self.inner.transport.connect().await?;
                let params = self.st().login_params.clone().ok_or_else(|| Error::session("cannot reconnect: no stored credentials"))?;
                let mut params = params;
                params.auto_heartbeat = true;
                self.login_with(params).await?;
                let (ids, book_ids) = { let st = self.st(); (st.subscribed_ids.clone(), st.subscribed_book_ids.clone()) };
                if !ids.is_empty() { self.subscribe_ticks(&ids).await?; }
                if !book_ids.is_empty() { self.subscribe_book(&book_ids).await?; }
                Ok(())
            }
            .await;
            match attempt_result {
                Ok(()) => {
                    {
                        let mut st = self.st();
                        st.reconnect_count += 1;
                        st.connected_at = Some(Instant::now());
                    }
                    info!("reconnected successfully on attempt {attempt}");
                    if let Some(m) = &cfg.metrics { m.on_reconnect_success(attempt); }
                    return;
                }
                Err(e) => warn!("reconnect attempt {attempt} failed: {e}"),
            }
        }
        error!("all {} reconnect attempts exhausted", cfg.max_reconnect_attempts);
    }

    // ---- health ----

    /// Snapshot of connection health (pings when ready to measure latency).
    pub async fn health_check(&self) -> HealthStatus {
        let mut latency = None;
        if self.inner.transport.is_ready() {
            let t0 = Instant::now();
            if self.ping().await.is_ok() { latency = Some(t0.elapsed().as_secs_f64() * 1000.0); }
        }
        let (uptime, reconnect_count, threshold) = {
            let st = self.st();
            (st.connected_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0), st.reconnect_count, st.health_threshold_ms)
        };
        let status = HealthStatus {
            state: self.inner.transport.state(),
            ping_latency_ms: latency,
            last_message_age_secs: self.inner.transport.last_message_at().map(|t| t.elapsed().as_secs_f64()),
            uptime_seconds: uptime,
            reconnect_count,
        };
        if latency.is_some_and(|l| l > threshold) {
            let cbs = self.inner.health_callbacks.lock().unwrap().clone();
            for cb in cbs { cb(&status); }
        }
        status
    }

    /// Callback fired from `health_check` when ping latency exceeds `threshold_ms`.
    pub fn on_health_degraded(&self, cb: impl Fn(&HealthStatus) + Send + Sync + 'static, threshold_ms: f64) {
        self.st().health_threshold_ms = threshold_ms;
        self.inner.health_callbacks.lock().unwrap().push(Arc::new(cb));
    }

    // ---- raw commands / session ----

    /// Escape hatch for reserved or reverse-engineered commands.
    pub async fn send_raw_command(&self, command: u16, payload: &[u8]) -> Result<CommandResult> {
        if matches!(command, CMD_INIT | CMD_LOGIN | 52) { self.st().bootstrap_pristine = false; }
        self.inner.transport.send_command(command, payload).await
    }

    /// Reserved bootstrap-only `cmd=52`. Only safe on a fresh connection before init/login
    /// (observed on Web Terminal build 5687: after cmd 28/29 the server drops the socket).
    pub async fn send_bootstrap_command_52(&self) -> Result<CommandResult> {
        if !self.inner.transport.is_ready() { return Err(Error::session("transport not ready")); }
        {
            let mut st = self.st();
            if st.logged_in || !st.bootstrap_pristine {
                return Err(Error::session(
                    "cmd=52 is only safe on a fresh bootstrap-only connection; create a new client and call it before init_session() or login()",
                ));
            }
            st.bootstrap_pristine = false;
        }
        self.inner.transport.send_command(52, &[]).await
    }

    pub async fn init_session(&self, version: u32, password: &str, otp: &str, cid: Option<&[u8]>) -> Result<CommandResult> {
        self.st().bootstrap_pristine = false;
        let payload = self.build_init_payload(version, password, otp, cid)?;
        self.inner.transport.send_command(CMD_INIT, &payload).await
    }

    /// Log in with default options. Returns `(token_hex, session_id)`.
    pub async fn login(&self, login: u64, password: &str) -> Result<(String, u64)> {
        self.login_with(LoginParams::new(login, password)).await
    }

    pub async fn login_with(&self, p: LoginParams) -> Result<(String, u64)> {
        self.st().bootstrap_pristine = false;
        let payload = self.build_login_payload(&p)?;
        let result = self.inner.transport.send_command(CMD_LOGIN, &payload).await?;
        if result.code != 0 {
            // pymt5 does not check this and fails later with an opaque "buffer too short".
            return Err(Error::Authentication(format!("login rejected by server: code={} (body {} bytes)", result.code, result.body.len())));
        }
        let vals = SeriesCodec::parse(&result.body, LOGIN_RESPONSE_SCHEMA, 0)?;
        let token = vals[0].as_bytes().unwrap_or(&[]).to_vec();
        let session_id = vals[1].as_u64().unwrap_or(0);
        {
            let mut st = self.st();
            st.logged_in = true;
            st.login_params = Some(p.clone());
        }
        info!("logged in: login={} session={}", p.login, session_id);
        if p.auto_heartbeat { self.start_heartbeat(); }
        Ok((bytes_to_hex(&token), session_id))
    }

    pub async fn ping(&self) -> Result<()> {
        self.inner.transport.send_command(CMD_PING, &[]).await?;
        Ok(())
    }

    pub async fn logout(&self) -> Result<()> {
        self.stop_heartbeat();
        self.inner.transport.send_command(CMD_LOGOUT, &[]).await?;
        {
            let mut st = self.st();
            st.logged_in = false;
            st.bootstrap_pristine = false;
        }
        info!("logged out");
        Ok(())
    }

    // ---- payload builders ----

    pub(crate) fn resolve_client_id(&self, cid: Option<&[u8]>) -> Result<[u8; 16]> {
        match cid {
            Some(c) => <[u8; 16]>::try_from(c).map_err(|_| Error::validation(format!("cid must be 16 bytes, got {}", c.len()))),
            None => Ok(build_client_id(if cfg!(windows) { "nt" } else { "posix" }, "1", "en-US", "0x0", None)),
        }
    }

    /// `(password_prefix, password_blob)`: a 320-char value is a hex encoded blob.
    pub(crate) fn split_password_blob(password: &str) -> Result<(String, Option<Vec<u8>>)> {
        if password.chars().count() == 320 {
            return Ok((String::new(), Some(hex_to_bytes(password)?)));
        }
        Ok((trunc(password, 32), None))
    }

    pub(crate) fn build_login_payload(&self, p: &LoginParams) -> Result<Vec<u8>> {
        let cid = self.resolve_client_id(p.cid.as_deref())?;
        let (prefix, blob) = Self::split_password_blob(&p.password)?;
        let url_len = p.url.chars().count().min(128) as u32;
        SeriesCodec::serialize(&[
            SerField::u32(p.version),
            SerField::fixed_string(&prefix, 64),
            SerField::fixed_string(&trunc(&p.otp, 64), 128),
            SerField::bytes(&cid, 16),
            SerField::fixed_string(&trunc(&p.utm_campaign, 32), 64),
            SerField::fixed_string(&trunc(&p.utm_source, 32), 64),
            SerField::u64(p.lead_cookie_id),
            SerField::fixed_string(&trunc(&p.lead_affiliate_site, 64), 128),
            SerField::u32(url_len),
            SerField::fixed_string(&trunc(&p.url, 128), 256),
            SerField::u64(p.login),
            SerField::bytes(&blob.unwrap_or_else(|| vec![0; 160]), 160),
            SerField::u64(p.session),
        ])
    }

    pub(crate) fn build_init_payload(&self, version: u32, password: &str, otp: &str, cid: Option<&[u8]>) -> Result<Vec<u8>> {
        let cid = self.resolve_client_id(cid)?;
        SeriesCodec::serialize(&[
            SerField::u32(version),
            SerField::fixed_string(&trunc(password, 32), 64),
            SerField::fixed_string(&trunc(otp, 64), 128),
            SerField::bytes(&cid, 16),
            SerField::fixed_string("", 64),
            SerField::fixed_string("", 64),
            SerField::u64(0),
            SerField::fixed_string("", 128),
            SerField::u32(0),
            SerField::fixed_string("", 256),
            SerField::u64(0),
        ])
    }

    pub(crate) fn build_otp_setup_payload(
        &self,
        login: u64,
        password: &str,
        otp: &str,
        otp_secret: &str,
        otp_secret_check: &str,
        cid: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let cid = self.resolve_client_id(cid)?;
        let (prefix, blob) = Self::split_password_blob(password)?;
        let mut fields = vec![
            SerField::u32(5),
            SerField::u64(login),
            SerField::fixed_string(&prefix, 64),
            SerField::fixed_string(&trunc(otp, 64), 128),
            SerField::fixed_string(&trunc(otp_secret, 64), 128),
            SerField::fixed_string(&trunc(otp_secret_check, 64), 128),
            SerField::bytes(&cid, 16),
        ];
        if let Some(b) = blob { fields.push(SerField::raw(&b)); }
        SeriesCodec::serialize(&fields)
    }
}
