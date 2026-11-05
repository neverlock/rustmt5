//! Protocol constants (port of `pymt5.constants`). Integer constants are generated
//! from the Python source by `tools/gen_constants.py`.

pub use crate::constants_gen::*;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Command ids accepted by the transport.
pub const VALID_COMMANDS: &[u16] = &[
    0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 22, 23, 24, 25, 27, 28, 29, 30, 33, 34,
    37, 39, 40, 41, 42, 43, 44, 50, 51, 52, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112,
];

/// minutes -> protocol period code.
pub fn period_map() -> &'static HashMap<i64, u16> {
    static M: OnceLock<HashMap<i64, u16>> = OnceLock::new();
    M.get_or_init(|| {
        HashMap::from([
            (1, PERIOD_M1), (5, PERIOD_M5), (15, PERIOD_M15), (30, PERIOD_M30), (60, PERIOD_H1),
            (240, PERIOD_H4), (1440, PERIOD_D1), (10080, PERIOD_W1), (43200, PERIOD_MN1),
        ])
    })
}

/// protocol period code -> minutes.
pub fn period_minutes_map() -> &'static HashMap<i64, i64> {
    static M: OnceLock<HashMap<i64, i64>> = OnceLock::new();
    M.get_or_init(|| period_map().iter().map(|(m, c)| (*c as i64, *m)).collect())
}

pub const FOREX_CALC_MODES: &[i64] = &[0, 5];
pub const FUTURES_CALC_MODES: &[i64] = &[1, 33, 34];
pub const CFD_CALC_MODES: &[i64] = &[2, 3, 4, 32, 38];
pub const OPTION_CALC_MODES: &[i64] = &[35, 36];
pub const BOND_CALC_MODES: &[i64] = &[37, 39];

/// Human readable description of a trade return code.
pub fn retcode_description(code: u32) -> Option<&'static str> {
    Some(match code {
        0 => "OK",
        10004 => "Requote",
        10006 => "Request rejected",
        10007 => "Request cancelled by trader",
        10008 => "Order placed",
        10009 => "Request completed (done)",
        10010 => "Request partially completed",
        10011 => "Request processing error",
        10012 => "Request cancelled by timeout",
        10013 => "Invalid request",
        10014 => "Invalid volume",
        10015 => "Invalid price",
        10016 => "Invalid stops",
        10017 => "Trade disabled",
        10018 => "Market closed",
        10019 => "Not enough money",
        10020 => "Price changed",
        10021 => "No quotes for request processing",
        10022 => "Invalid order expiration",
        10023 => "Order state changed",
        10024 => "Too many requests",
        10025 => "No changes in request",
        10026 => "Auto-trading disabled by server",
        10027 => "Auto-trading disabled by client",
        10028 => "Request locked for processing",
        10029 => "Order/position frozen",
        10030 => "Invalid order filling type",
        10031 => "No connection with trade server",
        10032 => "Operation allowed only for live accounts",
        10033 => "Pending orders limit reached",
        10034 => "Volume limit for symbol reached",
        10035 => "Invalid or prohibited order type",
        10036 => "Position already closed",
        _ => return None,
    })
}

/// Description with the Python fallback text for unknown codes.
pub fn retcode_text(code: u32) -> String {
    retcode_description(code).map(str::to_string).unwrap_or_else(|| format!("Unknown retcode {code}"))
}
