//! Input validation helpers (port of `pymt5._validation`).

use crate::error::{Error, Result};

pub fn validate_volume(volume: f64) -> Result<()> {
    if volume <= 0.0 { return Err(Error::validation(format!("volume must be > 0, got {volume}"))); }
    Ok(())
}

pub fn validate_price(price: f64, name: &str) -> Result<()> {
    if price <= 0.0 { return Err(Error::validation(format!("{name} must be > 0, got {price}"))); }
    Ok(())
}

pub fn validate_symbol_name(name: &str) -> Result<()> {
    if name.trim().is_empty() { return Err(Error::validation("symbol name must be non-empty")); }
    if name.chars().any(|c| c.is_control()) {
        return Err(Error::validation(format!("symbol name contains non-printable characters: {name:?}")));
    }
    Ok(())
}

pub fn validate_connection_state(is_ready: bool, logged_in: bool) -> Result<()> {
    if !is_ready { return Err(Error::session("transport is not connected")); }
    if !logged_in { return Err(Error::session("not logged in")); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checks() {
        assert!(validate_volume(0.0).is_err());
        assert!(validate_volume(0.1).is_ok());
        assert!(validate_price(-1.0, "p").unwrap_err().to_string().contains("p must be > 0"));
        assert!(validate_symbol_name("  ").is_err());
        assert!(validate_symbol_name("EUR\nUSD").is_err());
        assert!(validate_symbol_name("EURUSD").is_ok());
        assert!(validate_connection_state(false, true).is_err());
        assert!(validate_connection_state(true, false).is_err());
        assert!(validate_connection_state(true, true).is_ok());
    }
}
