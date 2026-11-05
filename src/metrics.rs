//! Optional operational metrics hook (port of `pymt5._metrics`).

/// Implement and pass in [`crate::ClientConfig`] to receive counters. All methods default to no-ops.
pub trait MetricsCollector: Send + Sync {
    fn on_command_sent(&self, _command: u16) {}
    fn on_command_received(&self, _command: u16, _code: u8) {}
    fn on_connect(&self) {}
    fn on_disconnect(&self, _reason: &str) {}
    fn on_reconnect_attempt(&self, _attempt: u32) {}
    fn on_reconnect_success(&self, _attempt: u32) {}
}
