//! Error hierarchy (port of `pymt5.exceptions`).

use thiserror::Error;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// All errors produced by this crate.
#[derive(Debug, Error)]
pub enum Error {
    /// Connection to the MT5 server failed or the socket dropped.
    #[error("connection error: {message}")]
    Connection { message: String, server_uri: String },
    /// Login / authentication failed.
    #[error("authentication error: {0}")]
    Authentication(String),
    /// A trade operation failed.
    #[error("trade error: {message} (retcode={retcode}, symbol={symbol}, action={action})")]
    Trade { message: String, retcode: u32, symbol: String, action: u32 },
    /// Binary protocol parsing failed.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Symbol not found in cache.
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    /// Input validation failed.
    #[error("validation error: {0}")]
    Validation(String),
    /// Operation requires a different session state.
    #[error("session error: {0}")]
    Session(String),
    /// A command timed out.
    #[error("timeout: {0}")]
    Timeout(String),
    /// WebSocket-level error.
    #[error("websocket error: {0}")]
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),
    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn protocol(msg: impl Into<String>) -> Self { Error::Protocol(msg.into()) }
    pub fn validation(msg: impl Into<String>) -> Self { Error::Validation(msg.into()) }
    pub fn session(msg: impl Into<String>) -> Self { Error::Session(msg.into()) }
    pub fn connection(msg: impl Into<String>) -> Self {
        Error::Connection { message: msg.into(), server_uri: String::new() }
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self { Error::WebSocket(Box::new(e)) }
}
