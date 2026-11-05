//! Binary-response parsing and helper functions (port of `pymt5._parsers`).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::constants::*;
use crate::protocol::{get_series_size, Record, RecordList, SeriesCodec, Value};
use crate::schemas::*;
use crate::types::{OpenAccountResult, SymbolInfo, VerificationStatus};

pub const BUY_ORDER_TYPES: &[i64] = &[0, 2, 4, 6];
pub const SELL_ORDER_TYPES: &[i64] = &[1, 3, 5, 7];

/// `Some(true)` for buy-side order types, `Some(false)` for sell-side, `None` otherwise.
pub fn order_side(action: i64) -> Option<bool> {
    if BUY_ORDER_TYPES.contains(&action) { Some(true) } else if SELL_ORDER_TYPES.contains(&action) { Some(false) } else { None }
}

pub fn currencies_equal(left: &str, right: &str) -> bool {
    left == right || (matches!(left, "RUB" | "RUR") && matches!(right, "RUB" | "RUR"))
}

// ---- timestamps ----

/// Flexible time argument: unix seconds (int/float) or a [`SystemTime`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeArg {
    Secs(i64),
    Float(f64),
    Time(SystemTime),
}
impl From<i64> for TimeArg { fn from(v: i64) -> Self { TimeArg::Secs(v) } }
impl From<i32> for TimeArg { fn from(v: i32) -> Self { TimeArg::Secs(v as i64) } }
impl From<u32> for TimeArg { fn from(v: u32) -> Self { TimeArg::Secs(v as i64) } }
impl From<u64> for TimeArg { fn from(v: u64) -> Self { TimeArg::Secs(v as i64) } }
impl From<f64> for TimeArg { fn from(v: f64) -> Self { TimeArg::Float(v) } }
impl From<SystemTime> for TimeArg { fn from(v: SystemTime) -> Self { TimeArg::Time(v) } }

fn system_time_secs_f64(t: SystemTime) -> f64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

pub fn coerce_timestamp(v: TimeArg) -> i64 {
    match v {
        TimeArg::Secs(s) => s,
        TimeArg::Float(f) => f as i64,
        TimeArg::Time(t) => system_time_secs_f64(t) as i64,
    }
}
pub fn coerce_optional_timestamp(v: Option<TimeArg>) -> i64 { v.map(coerce_timestamp).unwrap_or(0) }

/// Coerce a dynamic [`Value`] (missing/null -> 0).
pub fn coerce_value_timestamp(v: Option<&Value>) -> i64 { v.and_then(Value::as_i64).unwrap_or(0) }

pub fn coerce_timestamp_ms(v: TimeArg) -> i64 {
    match v {
        TimeArg::Time(t) => (system_time_secs_f64(t) * 1000.0) as i64,
        TimeArg::Float(f) => (f * 1000.0) as i64,
        TimeArg::Secs(s) => s * 1000,
    }
}
pub fn coerce_timestamp_ms_end(v: TimeArg) -> i64 {
    match v {
        TimeArg::Secs(s) => s * 1000 + 999,
        TimeArg::Float(f) if f.fract() == 0.0 => (f as i64) * 1000 + 999,
        other => coerce_timestamp_ms(other),
    }
}

pub fn normalize_timeframe_minutes(timeframe: i64) -> i64 {
    period_minutes_map().get(&timeframe).copied().unwrap_or(timeframe)
}
pub fn history_lookback_seconds(period_minutes: i64, bars: i64) -> i64 {
    (period_minutes * 60).max(60) * (bars * 4 + 8).max(16)
}

// ---- glob matching (Python fnmatch.fnmatchcase) ----

pub fn fnmatchcase(name: &str, pattern: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    fn rec(n: &[char], p: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| rec(&n[i..], &p[1..])),
            Some('?') => !n.is_empty() && rec(&n[1..], &p[1..]),
            Some('[') => {
                // find closing bracket (a ']' right after '[' or '[!' is literal)
                let mut j = 1;
                if p.get(j) == Some(&'!') { j += 1; }
                if p.get(j) == Some(&']') { j += 1; }
                while j < p.len() && p[j] != ']' { j += 1; }
                if j >= p.len() {
                    return !n.is_empty() && n[0] == '[' && rec(&n[1..], &p[1..]);
                }
                if n.is_empty() { return false; }
                let mut set = &p[1..j];
                let negate = set.first() == Some(&'!');
                if negate { set = &set[1..]; }
                let c = n[0];
                let mut hit = false;
                let mut k = 0;
                while k < set.len() {
                    if k + 2 < set.len() && set[k + 1] == '-' {
                        if set[k] <= c && c <= set[k + 2] { hit = true; }
                        k += 3;
                    } else {
                        if set[k] == c { hit = true; }
                        k += 1;
                    }
                }
                hit != negate && rec(&n[1..], &p[j + 1..])
            }
            Some(&c) => !n.is_empty() && n[0] == c && rec(&n[1..], &p[1..]),
        }
    }
    rec(&n, &p)
}

/// Comma separated glob mask; `!pattern` entries exclude.
pub fn matches_group_mask(symbol: &str, group: &str) -> bool {
    let patterns: Vec<&str> = group.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if patterns.is_empty() { return true; }
    let positive: Vec<&str> = patterns.iter().copied().filter(|p| !p.starts_with('!')).collect();
    let negative: Vec<&str> = patterns.iter().filter_map(|p| p.strip_prefix('!')).collect();
    let included = if positive.is_empty() { true } else { positive.iter().any(|p| fnmatchcase(symbol, p)) };
    if !included { return false; }
    !negative.iter().any(|p| fnmatchcase(symbol, p))
}

// ---- full symbol normalisation ----

fn u16_pair(buf: &[u8], off: usize) -> Value {
    Value::List(vec![
        Value::UInt(u16::from_le_bytes([buf[off], buf[off + 1]]) as u64),
        Value::UInt(u16::from_le_bytes([buf[off + 2], buf[off + 3]]) as u64),
    ])
}

pub fn parse_full_symbol_schedule(buffer: &[u8]) -> Record {
    let mut quote = Vec::new();
    let mut trade = Vec::new();
    if !buffer.is_empty() {
        let mut offset = 0;
        for target in [&mut quote, &mut trade] {
            for _ in 0..7 {
                let mut day = Vec::new();
                for _ in 0..16 {
                    if offset + 4 > buffer.len() { break; }
                    day.push(u16_pair(buffer, offset));
                    offset += 4;
                }
                target.push(Value::List(day));
            }
        }
    }
    Record::new().with("quote_sessions", Value::List(quote)).with("trade_sessions", Value::List(trade))
}

pub fn parse_full_symbol_subscription(buffer: &[u8]) -> Record {
    if buffer.len() < 8 {
        return Record::new().with("delay", 0u32).with("status", 0u32).with("level", 0u32).with("reserved", 0u32);
    }
    Record::new()
        .with("delay", u32::from_le_bytes(buffer[0..4].try_into().unwrap()))
        .with("status", buffer[4] as u32)
        .with("level", buffer[5] as u32)
        .with("reserved", u16::from_le_bytes([buffer[6], buffer[7]]) as u32)
}

fn pick(trade: &Record, info: &Record, trade_key: &str, info_key: &str, default: Value) -> Value {
    trade.get(trade_key).or_else(|| info.get(info_key)).cloned().unwrap_or(default)
}

pub fn normalize_full_symbol_record(record: &Record) -> Record {
    let mut info = record.clone();
    let trade_rec: Record = match info.get("trade") {
        Some(Value::Bytes(b)) => {
            let size = get_series_size(ACCOUNT_WEB_TRADE_SETTINGS_SCHEMA).unwrap_or(usize::MAX);
            if b.len() >= size { parse_account_trade_settings_record(b, 0) } else { Record::new() }
        }
        Some(Value::Record(r)) => r.clone(),
        _ => Record::new(),
    };
    info.set("trade", trade_rec.clone());
    if let Some(Value::Bytes(b)) = info.get("schedule").cloned() { info.set("schedule", parse_full_symbol_schedule(&b)); }
    if let Some(Value::Bytes(b)) = info.get("subscription").cloned() { info.set("subscription", parse_full_symbol_subscription(&b)); }
    let t = &trade_rec;
    let i2 = info.clone();
    let z = || Value::Float(0.0);
    let zi = || Value::Int(0);
    info.set("symbol_path", pick(t, &i2, "symbol_path", "symbol_path", Value::Str(String::new())));
    info.set("trade_mode", pick(t, &i2, "trade_mode", "trade_mode", zi()));
    info.set("trade_stops_level", pick(t, &i2, "trade_stops_level", "trade_stops_level", zi()));
    info.set("trade_freeze_level", pick(t, &i2, "trade_freeze_level", "trade_freeze_level", zi()));
    for k in ["volume_min", "volume_max", "volume_step", "volume_limit", "margin_initial", "margin_maintenance"] {
        info.set(k, pick(t, &i2, k, k, z()));
    }
    info.set("filling_mode", pick(t, &i2, "trade_fill_flags", "filling_mode", zi()));
    info.set("expiration_mode", pick(t, &i2, "trade_time_flags", "expiration_mode", zi()));
    info.set("order_mode", pick(t, &i2, "trade_order_flags", "order_mode", zi()));
    let fv = i2.get("face_value").cloned().unwrap_or_else(z);
    let ai = i2.get("accrued_interest").cloned().unwrap_or_else(z);
    info.set("trade_face_value", fv);
    info.set("trade_accrued_interest", ai);
    info
}

// ---- tick copy helpers ----

pub fn tick_matches_copy_flags(tick: &Record, flags: u32) -> bool {
    if flags == COPY_TICKS_ALL { return true; }
    let has_quote = tick.get_f64("bid") != 0.0 || tick.get_f64("ask") != 0.0;
    let has_trade = tick.get_f64("last") != 0.0 || tick.get_i64("tick_volume") != 0;
    if flags & COPY_TICKS_INFO != 0 && has_quote { return true; }
    flags & COPY_TICKS_TRADE != 0 && has_trade
}

pub fn to_copy_tick_record(tick: &Record) -> Record {
    Record::new()
        .with("time", tick.get_i64("tick_time"))
        .with("time_msc", tick.get_i64("tick_time_ms"))
        .with("bid", tick.get_f64("bid"))
        .with("ask", tick.get_f64("ask"))
        .with("last", tick.get_f64("last"))
        .with("volume", tick.get_i64("tick_volume"))
        .with("volume_real", tick.get_i64("tick_volume") as f64)
        .with("flags", tick.get_i64("flags"))
        .with("symbol", tick.get_str("symbol"))
}

// ---- order request validation ----

pub fn validate_requested_volume(symbol_info: &Record, volume: f64) -> Option<String> {
    let vmin = symbol_info.get_f64("volume_min");
    let vmax = symbol_info.get_f64("volume_max");
    let step = symbol_info.get_f64("volume_step");
    if vmin > 0.0 && volume < vmin { return Some(format!("volume {volume} is below symbol minimum {vmin}")); }
    if vmax > 0.0 && volume > vmax { return Some(format!("volume {volume} is above symbol maximum {vmax}")); }
    if step > 0.0 && vmin >= 0.0 {
        let steps = if volume >= vmin { (volume - vmin) / step } else { volume / step };
        if (steps - steps.round_ties_even()).abs() > 1e-9 {
            return Some(format!("volume {volume} does not align with step {step}"));
        }
    }
    None
}

pub fn validate_requested_stops(symbol_info: &Record, request: &Record, reference_price: f64) -> Option<String> {
    let point = symbol_info.get_f64("point");
    let stops_level = symbol_info.get_f64("trade_stops_level");
    if point <= 0.0 || stops_level <= 0.0 { return None; }
    let min_distance = point * stops_level;
    for label in ["sl", "tp"] {
        let v = request.get_f64(label);
        if v > 0.0 && (reference_price - v).abs() < min_distance {
            return Some(format!("{label} is closer than symbol stop level"));
        }
    }
    if request.get_i64("action") == TRADE_ACTION_PENDING {
        let trig = request.get_f64("price_trigger");
        if trig > 0.0 && (reference_price - trig).abs() < min_distance {
            return Some("pending trigger is closer than symbol stop level".into());
        }
    }
    None
}

// ---- ticks / book ----

/// Undo the protocol's scaled-integer price encoding where detected.
pub fn normalize_tick_price_value(value: f64, digits: u32, symbol: &str) -> f64 {
    let price = value;
    if digits == 0 || price == 0.0 { return price; }
    let scale = 10f64.powi(digits as i32);
    let upper = symbol.to_uppercase();
    let base: String = upper.chars().take(6).collect();
    let is_forex_like = base.chars().count() == 6 && base.chars().all(char::is_alphabetic);

    let mut threshold = scale;
    if digits >= 4 && is_forex_like && !base.contains("JPY") { threshold = scale / 10.0; }

    if is_forex_like && digits >= 3 {
        return if price.abs() >= threshold { price / scale } else { price };
    }
    let rounded = price.round_ties_even();
    if (price - rounded).abs() > 1e-9 { return price; }
    if rounded.abs() < threshold { return price; }
    rounded / scale
}

/// Parse a CMD_TICK_PUSH body. Zero bid/ask/last (meaning "unchanged") are filled from `cache`.
pub fn parse_tick_batch(
    body: &[u8],
    symbols_by_id: &HashMap<u32, SymbolInfo>,
    tick_cache_by_id: Option<&HashMap<u32, Record>>,
) -> RecordList {
    let tick_size = get_series_size(TICK_SCHEMA).expect("static schema");
    let count = body.len() / tick_size;
    let mut ticks = Vec::with_capacity(count);
    for idx in 0..count {
        let Ok(mut tick) = SeriesCodec::parse_record(body, TICK_SCHEMA, TICK_FIELD_NAMES, idx * tick_size) else { break };
        let ms = tick.get_i64("tick_time") * 1000 + tick.get_i64("time_ms_delta");
        tick.set("tick_time_ms", ms);
        let sid = tick.get_u64("symbol_id") as u32;
        if let Some(sym) = symbols_by_id.get(&sid) {
            tick.set("symbol", sym.name.as_str());
            for k in ["bid", "ask", "last"] {
                let v = normalize_tick_price_value(tick.get_f64(k), sym.digits, &sym.name);
                tick.set(k, v);
            }
        }
        if let Some(cached) = tick_cache_by_id.and_then(|c| c.get(&sid)) {
            for k in ["bid", "ask", "last"] {
                if tick.get_f64(k) == 0.0 {
                    let prev = cached.get_f64(k);
                    if prev != 0.0 { tick.set(k, prev); }
                }
            }
        }
        ticks.push(tick);
    }
    ticks
}

fn read_u32(body: &[u8], off: usize) -> Option<u32> { body.get(off..off + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())) }
fn read_i32(body: &[u8], off: usize) -> Option<i32> { body.get(off..off + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap())) }

pub fn parse_book_entries(body: &[u8], symbols_by_id: &HashMap<u32, SymbolInfo>) -> RecordList {
    if body.len() < 4 { return vec![]; }
    let header_size = get_series_size(BOOK_HEADER_SCHEMA).unwrap();
    let level_size = get_series_size(BOOK_LEVEL_SCHEMA).unwrap();
    let count = read_u32(body, 0).unwrap();
    let mut entries = Vec::new();
    let mut offset = 4;
    for _ in 0..count {
        if offset + header_size > body.len() { break; }
        let Ok(header) = SeriesCodec::parse_record(body, BOOK_HEADER_SCHEMA, BOOK_HEADER_FIELD_NAMES, offset) else { break };
        offset += header_size;
        let bid_count = header.get_u64("bid_count") as usize;
        let total = bid_count + header.get_u64("ask_count") as usize;
        let mut levels: Vec<Record> = Vec::new();
        for _ in 0..total {
            if offset + level_size > body.len() { break; }
            let Ok(l) = SeriesCodec::parse_record(body, BOOK_LEVEL_SCHEMA, BOOK_LEVEL_FIELD_NAMES, offset) else { break };
            levels.push(l);
            offset += level_size;
        }
        let split = bid_count.min(levels.len());
        let asks = levels.split_off(split);
        let mut entry = Record::new().with("symbol_id", header.get("symbol_id").cloned().unwrap_or_default());
        entry.set("bids", levels);
        entry.set("asks", asks);
        if let Some(sym) = symbols_by_id.get(&(header.get_u64("symbol_id") as u32)) {
            entry.set("symbol", sym.name.as_str());
        }
        entries.push(entry);
    }
    entries
}

// ---- account response ----

pub fn parse_f64_array(buffer: &[u8], expected_count: usize) -> Vec<f64> {
    let count = expected_count.min(buffer.len() / 8);
    (0..count).map(|i| f64::from_le_bytes(buffer[i * 8..i * 8 + 8].try_into().unwrap())).collect()
}

pub fn parse_account_trade_settings_record(body: &[u8], offset: usize) -> Record {
    let mut item = SeriesCodec::parse_record(body, ACCOUNT_WEB_TRADE_SETTINGS_SCHEMA, ACCOUNT_WEB_TRADE_SETTINGS_FIELD_NAMES, offset)
        .unwrap_or_default();
    for (key, n) in [("margin_rates_initial", 8), ("margin_rates_maintenance", 8), ("swap_rates", 7)] {
        let arr = parse_f64_array(item.get(key).and_then(Value::as_bytes).unwrap_or(&[]), n);
        item.set(key, arr);
    }
    item
}

fn parse_account_trade_settings(body: &[u8], mut offset: usize) -> (RecordList, usize) {
    let mut items = Vec::new();
    let size = get_series_size(ACCOUNT_WEB_TRADE_SETTINGS_SCHEMA).unwrap();
    let Some(count) = read_u32(body, offset) else { return (items, offset) };
    // NB: parity with pymt5 - an empty count is *not* consumed (offset stays put).
    if count == 0 { return (items, offset); }
    offset += 4;
    for _ in 0..count {
        if offset + size > body.len() { break; }
        items.push(parse_account_trade_settings_record(body, offset));
        offset += size;
    }
    (items, offset)
}

fn parse_account_leverage_rules(body: &[u8], mut offset: usize) -> (RecordList, usize) {
    let mut rules = Vec::new();
    let rule_size = get_series_size(ACCOUNT_WEB_LEVERAGE_RULE_SCHEMA).unwrap();
    let tier_size = get_series_size(ACCOUNT_WEB_LEVERAGE_TIER_SCHEMA).unwrap();
    let Some(count) = read_i32(body, offset) else { return (rules, offset) };
    offset += 4;
    for _ in 0..count.max(0) {
        if offset + rule_size > body.len() { break; }
        let Ok(mut rule) = SeriesCodec::parse_record(body, ACCOUNT_WEB_LEVERAGE_RULE_SCHEMA, ACCOUNT_WEB_LEVERAGE_RULE_FIELD_NAMES, offset) else { break };
        offset += rule_size;
        let mut tiers = Vec::new();
        for _ in 0..rule.get_i64("tier_count").max(0) {
            if offset + tier_size > body.len() { break; }
            let Ok(t) = SeriesCodec::parse_record(body, ACCOUNT_WEB_LEVERAGE_TIER_SCHEMA, ACCOUNT_WEB_LEVERAGE_TIER_FIELD_NAMES, offset) else { break };
            tiers.push(t);
            offset += tier_size;
        }
        rule.set("tiers", tiers);
        rules.push(rule);
    }
    (rules, offset)
}

fn parse_account_commissions(body: &[u8], mut offset: usize) -> (RecordList, usize) {
    let mut out = Vec::new();
    let size = get_series_size(ACCOUNT_WEB_COMMISSION_SCHEMA).unwrap();
    let tier_size = get_series_size(ACCOUNT_WEB_COMMISSION_TIER_SCHEMA).unwrap();
    let Some(count) = read_u32(body, offset) else { return (out, offset) };
    offset += 4;
    for _ in 0..count {
        if offset + size > body.len() { break; }
        let Ok(mut c) = SeriesCodec::parse_record(body, ACCOUNT_WEB_COMMISSION_SCHEMA, ACCOUNT_WEB_COMMISSION_FIELD_NAMES, offset) else { break };
        offset += size;
        let mut tiers = Vec::new();
        let Some(tier_count) = read_u32(body, offset) else {
            c.set("tiers", tiers);
            out.push(c);
            break;
        };
        offset += 4;
        for _ in 0..tier_count {
            if offset + tier_size > body.len() { break; }
            let Ok(t) = SeriesCodec::parse_record(body, ACCOUNT_WEB_COMMISSION_TIER_SCHEMA, ACCOUNT_WEB_COMMISSION_TIER_FIELD_NAMES, offset) else { break };
            tiers.push(t);
            offset += tier_size;
        }
        c.set("tiers", tiers);
        out.push(c);
    }
    (out, offset)
}

/// Parse the cmd=3 / cmd=14 account response. Returns an empty record on malformed data.
pub fn parse_account_response(body: &[u8]) -> Record {
    let Ok(min_size) = get_series_size(ACCOUNT_WEB_MAIN_SCHEMA) else { return Record::new() };
    if body.len() < min_size { return Record::new(); }
    let Ok(mut r) = SeriesCodec::parse_record(body, ACCOUNT_WEB_MAIN_SCHEMA, ACCOUNT_WEB_MAIN_FIELD_NAMES, 0) else { return Record::new() };
    let apm = r.get_i64("auth_password_min");
    r.set("auth_password_min", if apm != 0 { apm } else { 8 });
    let mut offset = min_size;
    let (trade_settings, o) = parse_account_trade_settings(body, offset);
    offset = o;
    r.set("trade_settings", trade_settings);
    r.set("trade_flags", 0i64);
    if let Some(v) = read_i32(body, offset) { r.set("trade_flags", v); offset += 4; }
    r.set("symbols_count", 0i64);
    if let Some(v) = read_i32(body, offset) { r.set("symbols_count", v); offset += 4; }
    r.set("leverage_flags", 0u64);
    if let Some(b) = body.get(offset..offset + 8) { r.set("leverage_flags", u64::from_le_bytes(b.try_into().unwrap())); offset += 8; }
    let (rules, o) = parse_account_leverage_rules(body, offset);
    offset = o;
    let (commissions, _) = parse_account_commissions(body, offset);
    r.set("leverage_rules", rules.clone());
    r.set("rules", rules);
    r.set("commissions", commissions);
    r.set("currency", r.get("account_currency").cloned().unwrap_or(Value::Str(String::new())));
    r.set("leverage", r.get_i64("margin_leverage"));
    r.set("name", r.get("account_name").cloned().unwrap_or(Value::Str(String::new())));
    r.set("server", r.get("server_name").cloned().unwrap_or(Value::Str(String::new())));
    r.set("daylight_mode", r.get_i64("daylightmode"));
    r.set("server_offset_time", r.get_i64("timezone_shift") + r.get_i64("daylight_mode") * 3600);
    let account_type = r.get("account_type").and_then(Value::as_i64).unwrap_or(-1);
    r.set("is_real", account_type == 0);
    r.set("is_demo", account_type == 1);
    r.set("is_contest", account_type == 2);
    r.set("is_preliminary", account_type == 3);
    let rights = r.get_i64("rights");
    let trade_allowed = rights & 4 == 0;
    r.set("trade_allowed", trade_allowed);
    r.set("is_investor", rights & 8 != 0);
    r.set("is_read_only", rights & 512 != 0);
    r.set("is_reset_pass", rights & 1024 != 0);
    r.set("is_trade_disabled", !trade_allowed);
    r.set("is_hedged_margin", r.get_i64("margin_mode") == 2);
    r.set("risk_warning", r.get_i64("permissions_flags") & 16 != 0);
    r.set("profit", r.get_f64("acc_profit"));
    r.set("equity", r.get_f64("balance") + r.get_f64("credit") + r.get_f64("profit"));
    r
}

pub fn parse_verification_status(body: &[u8]) -> VerificationStatus {
    if body.len() < 2 { return VerificationStatus::default(); }
    VerificationStatus { email: body[0] != 0, phone: body[1] != 0 }
}

pub fn parse_open_account_result(body: &[u8]) -> OpenAccountResult {
    let fail = OpenAccountResult { code: -1, ..Default::default() };
    if body.len() < get_series_size(OPEN_ACCOUNT_RESPONSE_SCHEMA).unwrap() { return fail; }
    match SeriesCodec::parse(body, OPEN_ACCOUNT_RESPONSE_SCHEMA, 0) {
        Ok(v) => OpenAccountResult {
            code: v[0].as_i64().unwrap_or(0),
            login: v[1].as_i64().unwrap_or(0),
            password: v[2].as_str().unwrap_or("").to_string(),
            investor_password: v[3].as_str().unwrap_or("").to_string(),
        },
        Err(_) => fail,
    }
}

pub fn parse_rate_bars(body: &[u8]) -> RecordList {
    if body.is_empty() { return vec![]; }
    let std_size = get_series_size(RATE_BAR_SCHEMA).unwrap();
    let ext_size = get_series_size(RATE_BAR_SCHEMA_EXT).unwrap();
    let parse = |schema, names, size: usize| -> RecordList {
        (0..body.len() / size).filter_map(|i| SeriesCodec::parse_record(body, schema, names, i * size).ok()).collect()
    };
    // Plausible = every bar time within 2000..2100 and non-decreasing.
    let plausible = |bars: &RecordList| {
        let mut prev = 0;
        bars.iter().all(|b| {
            let t = b.get_i64("time");
            let ok = (946_684_800..4_102_444_800).contains(&t) && t >= prev;
            prev = t;
            ok
        })
    };
    let ext_fits = body.len().is_multiple_of(ext_size);
    let std_fits = body.len().is_multiple_of(std_size);
    if ext_fits && !std_fits {
        return parse(RATE_BAR_SCHEMA_EXT, RATE_BAR_FIELD_NAMES_EXT, ext_size);
    }
    if ext_fits && std_fits {
        // Ambiguous (length is a multiple of lcm(48,56)). pymt5 always picked the extended layout here,
        // which mis-parses standard servers; the standard 48-byte layout is preferred when plausible.
        let std_bars = parse(RATE_BAR_SCHEMA, RATE_BAR_FIELD_NAMES, std_size);
        if plausible(&std_bars) { return std_bars; }
        let ext_bars = parse(RATE_BAR_SCHEMA_EXT, RATE_BAR_FIELD_NAMES_EXT, ext_size);
        return if plausible(&ext_bars) { ext_bars } else { std_bars };
    }
    parse(RATE_BAR_SCHEMA, RATE_BAR_FIELD_NAMES, std_size)
}

/// Parse `[u32 count][record]*` bodies.
pub fn parse_counted_records(body: &[u8], schema: &[crate::protocol::Field], names: &[&str]) -> RecordList {
    let Some(count) = read_u32(body, 0) else { return vec![] };
    let size = get_series_size(schema).unwrap_or(usize::MAX);
    let mut out = Vec::new();
    let mut offset: usize = 4;
    for _ in 0..count {
        if offset.saturating_add(size) > body.len() { break; }
        match SeriesCodec::parse_record(body, schema, names, offset) {
            Ok(r) => out.push(r),
            Err(_) => break,
        }
        offset += size;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SerField;

    #[test]
    fn glob_and_group_masks() {
        assert!(fnmatchcase("EURUSD", "EUR*"));
        assert!(fnmatchcase("EURUSD", "EUR???"));
        assert!(!fnmatchcase("EURUSD", "eur*"));
        assert!(fnmatchcase("EURUSD", "[EG]*"));
        assert!(!fnmatchcase("EURUSD", "[!E]*"));
        assert!(fnmatchcase("A1", "A[0-9]"));
        assert!(matches_group_mask("EURUSD", "EUR*,GBP*"));
        assert!(!matches_group_mask("EURUSD", "EUR*,!*USD"));
        assert!(matches_group_mask("XAUUSD", "!EUR*"));
        assert!(matches_group_mask("ANY", " , "));
    }

    #[test]
    fn timestamps() {
        assert_eq!(coerce_timestamp(5i64.into()), 5);
        assert_eq!(coerce_timestamp_ms(5i64.into()), 5000);
        assert_eq!(coerce_timestamp_ms(1.5f64.into()), 1500);
        assert_eq!(coerce_timestamp_ms_end(5i64.into()), 5999);
        assert_eq!(coerce_timestamp_ms_end(5.0f64.into()), 5999);
        assert_eq!(coerce_timestamp_ms_end(1.5f64.into()), 1500);
        assert_eq!(coerce_optional_timestamp(None), 0);
        assert_eq!(history_lookback_seconds(1, 10), 60 * 48);
        assert_eq!(normalize_timeframe_minutes(16385), 60);
        assert_eq!(normalize_timeframe_minutes(7), 7);
    }

    #[test]
    fn tick_price_normalisation() {
        assert_eq!(normalize_tick_price_value(0.0, 5, "EURUSD"), 0.0);
        assert!((normalize_tick_price_value(115364.0, 5, "EURUSD") - 1.15364).abs() < 1e-12);
        assert_eq!(normalize_tick_price_value(1.15364, 5, "EURUSD"), 1.15364);
        assert!((normalize_tick_price_value(87154.5, 5, "EURUSD") - 0.871545).abs() < 1e-12);
        // parity with pymt5: integer-valued price >= 10^digits is treated as scaled
        assert_eq!(normalize_tick_price_value(2000.0, 2, "XAUUSD"), 20.0);
        assert_eq!(normalize_tick_price_value(2000.5, 2, "XAU"), 2000.5);
    }

    fn tick_bytes(sid: u32, bid: f64, ask: f64) -> Vec<u8> {
        SeriesCodec::serialize(&[
            SerField::u32(sid), SerField::i32(1_700_000_000), SerField::u32(3), SerField::f64(bid), SerField::f64(ask),
            SerField::f64(0.0), SerField::new(crate::protocol::PropType::I64, 7i64, None), SerField::u32(250), SerField::u16(2),
        ])
        .unwrap()
    }

    #[test]
    fn tick_batch_merges_cache_and_names() {
        let mut syms = HashMap::new();
        syms.insert(5, SymbolInfo { name: "EURUSD".into(), symbol_id: 5, digits: 5, ..Default::default() });
        let mut body = tick_bytes(5, 1.1, 1.2);
        body.extend(tick_bytes(5, 0.0, 1.3));
        let mut cache = HashMap::new();
        cache.insert(5u32, Record::new().with("bid", 1.05f64).with("last", 9.0f64));
        let ticks = parse_tick_batch(&body, &syms, Some(&cache));
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].get_str("symbol"), "EURUSD");
        assert_eq!(ticks[0].get_i64("tick_time_ms"), 1_700_000_000_250);
        assert_eq!(ticks[1].get_f64("bid"), 1.05); // carried forward
        assert_eq!(ticks[1].get_f64("last"), 9.0);
        assert_eq!(ticks[1].get_f64("ask"), 1.3);
        assert!(parse_tick_batch(&[], &syms, None).is_empty());
    }

    #[test]
    fn rate_bars_std_and_ext() {
        let mut b = Vec::new();
        b.extend(1_773_293_460i32.to_le_bytes());
        for v in [1.15364f64, 1.15369, 1.15359, 1.15364] { b.extend(v.to_le_bytes()); }
        b.extend(57i64.to_le_bytes());
        b.extend(0i32.to_le_bytes());
        let mut two = b.clone();
        two.extend(&b);
        assert_eq!(b.len(), 48);
        let bars = parse_rate_bars(&two);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].get_i64("time"), 1_773_293_460);
        assert_eq!(bars[0].get_i64("tick_volume"), 57);
        assert!(parse_rate_bars(&[]).is_empty());
    }

    #[test]
    fn rate_bars_ambiguous_length_prefers_plausible_layout() {
        let bar = |t: i32, ext: bool| {
            let mut b = t.to_le_bytes().to_vec();
            for v in [1.0f64, 2.0, 0.5, 1.5] { b.extend(v.to_le_bytes()); }
            b.extend(10i64.to_le_bytes());
            b.extend(0i32.to_le_bytes());
            if ext { b.extend(99i64.to_le_bytes()); }
            b
        };
        // 7 std bars = 336 bytes = 6 ext bars: ambiguous, must parse as 7 standard bars
        let std7: Vec<u8> = (0..7).flat_map(|i| bar(1_700_000_000 + i * 3600, false)).collect();
        assert_eq!(std7.len(), 336);
        assert_eq!(parse_rate_bars(&std7).len(), 7);
        // 6 genuine ext bars (336 bytes) fail the std plausibility check and are parsed as ext
        let ext6: Vec<u8> = (0..6).flat_map(|i| bar(1_700_000_000 + i * 3600, true)).collect();
        let bars = parse_rate_bars(&ext6);
        assert_eq!(bars.len(), 6);
        assert_eq!(bars[5].get_i64("real_volume"), 99);
    }

    #[test]
    fn counted_records() {
        let rec = SeriesCodec::serialize(&[
            SerField::fixed_string("GBPUSD", 64), SerField::fixed_string("", 128), SerField::u32(5), SerField::u32(99),
            SerField::fixed_string("", 256), SerField::u32(0), SerField::fixed_string("USD", 64), SerField::u16(0),
        ])
        .unwrap();
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend(rec);
        let r = parse_counted_records(&body, SYMBOL_BASIC_SCHEMA, SYMBOL_BASIC_FIELD_NAMES);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].get_str("trade_symbol"), "GBPUSD");
        assert_eq!(r[0].get_u64("symbol_id"), 99);
        assert!(parse_counted_records(&[], SYMBOL_BASIC_SCHEMA, SYMBOL_BASIC_FIELD_NAMES).is_empty());
        assert!(parse_counted_records(&[0, 0, 0, 0], SYMBOL_BASIC_SCHEMA, SYMBOL_BASIC_FIELD_NAMES).is_empty());
    }

    #[test]
    fn book_entries() {
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend(SeriesCodec::serialize(&[
            SerField::u32(5), SerField::i32(0), SerField::i32(0), SerField::u32(1), SerField::u32(2), SerField::u16(0),
        ]).unwrap());
        for (p, v) in [(1.1f64, 10i64), (1.2, 20), (1.3, 30)] { body.extend(p.to_le_bytes()); body.extend(v.to_le_bytes()); }
        let e = parse_book_entries(&body, &HashMap::new());
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].get_records("bids").len(), 1);
        assert_eq!(e[0].get_records("asks").len(), 2);
        assert_eq!(e[0].get_records("asks")[1].get_i64("volume"), 30);
    }

    #[test]
    fn volume_and_stops_validation() {
        let info = Record::new().with("volume_min", 0.01f64).with("volume_max", 100.0f64).with("volume_step", 0.01f64)
            .with("point", 0.0001f64).with("trade_stops_level", 10.0f64);
        assert!(validate_requested_volume(&info, 0.5).is_none());
        assert!(validate_requested_volume(&info, 0.001).unwrap().contains("below"));
        assert!(validate_requested_volume(&info, 101.0).unwrap().contains("above"));
        assert!(validate_requested_volume(&info, 0.015).unwrap().contains("step"));
        let req = Record::new().with("sl", 1.0999f64);
        assert!(validate_requested_stops(&info, &req, 1.1).unwrap().contains("sl"));
        assert!(validate_requested_stops(&info, &Record::new().with("sl", 1.0f64), 1.1).is_none());
    }

    #[test]
    fn open_account_and_verification() {
        assert_eq!(parse_open_account_result(&[]).code, -1);
        let body = SeriesCodec::serialize(&[
            SerField::u32(0), SerField::new(crate::protocol::PropType::I64, 123i64, None),
            SerField::fixed_string("pw", 32), SerField::fixed_string("inv", 32),
        ]).unwrap();
        let r = parse_open_account_result(&body);
        assert!(r.success());
        assert_eq!((r.login, r.password.as_str(), r.investor_password.as_str()), (123, "pw", "inv"));
        assert!(parse_verification_status(&[1, 0]).any());
        assert!(!parse_verification_status(&[]).any());
    }

    #[test]
    fn account_response_derived_fields() {
        let mut body = vec![0u8; get_series_size(ACCOUNT_WEB_MAIN_SCHEMA).unwrap()];
        body.extend([0u8; 4]); // no trade settings
        assert_eq!(parse_account_response(&[0u8; 3]).len(), 0);
        let r = parse_account_response(&body);
        assert_eq!(r.get_i64("auth_password_min"), 8);
        assert!(r.get_bool("is_real"));
        assert!(r.get_bool("trade_allowed"));
        assert_eq!(r.get_f64("equity"), 0.0);
    }
}
