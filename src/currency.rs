//! Currency conversion, profit and margin formulas (port of `pymt5._currency`).

use crate::protocol::Value;
use tracing::debug;

use crate::client::{is_soft_error, Mt5Client};
use crate::constants::*;
use crate::error::Result;
use crate::protocol::RecordList;
use crate::parsers::currencies_equal;
use crate::protocol::Record;

/// `("USD", 100.0)` for the cent currency `USC` (1 USC = 1/100 USD), `(ccy, 1.0)` for everything else.
fn split_cent_currency(ccy: &str) -> (&str, f64) {
    match ccy {
        "USC" => ("USD", 100.0),
        "EUC" => ("EUR", 100.0),
        "GBC" => ("GBP", 100.0),
        "AUC" => ("AUD", 100.0),
        _ => (ccy, 1.0),
    }
}

impl Mt5Client {
    /// Buy/sell conversion rates `source -> target`; tries direct, then triangulates via USD.
    pub(crate) async fn resolve_conversion_rates(&self, source: &str, target: &str, current_symbol: &str, fallback: f64) -> Result<Option<(f64, f64)>> {
        if source.is_empty() || target.is_empty() { return Ok(None); }
        // Cent accounts (USC, EUC, ...) have no FX pairs: convert via the main currency and scale.
        let ((s_base, s_factor), (t_base, t_factor)) = (split_cent_currency(source), split_cent_currency(target));
        if (s_base, t_base) != (source, target) {
            let scale = t_factor / s_factor;
            return Ok(Box::pin(self.resolve_conversion_rates(s_base, t_base, current_symbol, fallback)).await?.map(|(b, s)| (b * scale, s * scale)));
        }
        if currencies_equal(source, target) { return Ok(Some((1.0, 1.0))); }
        let buy = self.resolve_side_rate(source, target, true, current_symbol, fallback).await?;
        let sell = self.resolve_side_rate(source, target, false, current_symbol, fallback).await?;
        if buy > 0.0 && sell > 0.0 { return Ok(Some((buy, sell))); }
        let buy_usd = self.resolve_side_rate(source, "USD", true, current_symbol, fallback).await?;
        let sell_usd = self.resolve_side_rate(source, "USD", false, current_symbol, fallback).await?;
        let buy_t = self.resolve_side_rate("USD", target, true, current_symbol, fallback).await?;
        let sell_t = self.resolve_side_rate("USD", target, false, current_symbol, fallback).await?;
        if buy_usd > 0.0 && sell_usd > 0.0 && buy_t > 0.0 && sell_t > 0.0 {
            return Ok(Some((buy_usd * buy_t, sell_usd * sell_t)));
        }
        Ok(None)
    }

    async fn resolve_side_rate(&self, source: &str, target: &str, prefer_ask_when_direct: bool, current_symbol: &str, fallback: f64) -> Result<f64> {
        if let Some(direct) = self.find_conversion_symbol_name(source, target).await? {
            if let Some((bid, ask)) = self.conversion_prices_subscribing(&direct, current_symbol, fallback).await {
                return Ok(if prefer_ask_when_direct { ask } else { bid });
            }
        }
        if let Some(inverse) = self.find_conversion_symbol_name(target, source).await? {
            if let Some((bid, ask)) = self.conversion_prices_subscribing(&inverse, current_symbol, fallback).await {
                let denominator = if prefer_ask_when_direct { bid } else { ask };
                if denominator > 0.0 { return Ok(1.0 / denominator); }
            }
        }
        Ok(0.0)
    }

    async fn find_conversion_symbol_name(&self, base: &str, quote: &str) -> Result<Option<String>> {
        if base.is_empty() || quote.is_empty() { return Ok(None); }
        if self.st().symbols.is_empty() {
            if let Err(e) = self.load_symbols(true).await {
                if is_soft_error(&e) {
                    debug!("load_symbols() failed in find_conversion_symbol_name: {e}");
                    return Ok(None);
                }
                return Err(e);
            }
        }
        let st = self.st();
        let direct = format!("{base}{quote}");
        if st.symbols.contains_key(&direct) { return Ok(Some(direct)); }
        let mut matches: Vec<&String> = st.symbols.keys().filter(|n| n.starts_with(base) && n.ends_with(quote)).collect();
        matches.sort();
        if let Some(m) = matches.first() { return Ok(Some((*m).clone())); }
        // Broker suffixes (e.g. "EURUSDp", "EURUSD.m"): pymt5's startswith/endswith rule misses these.
        // Prefer the shortest name (`XAUUSDs` over `XAUUSD247s`), then alphabetical.
        let suffixed = st.symbols.keys().filter(|n| n.starts_with(&direct)).min_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        Ok(suffixed.cloned())
    }

    /// Conversion prices; when the symbol has no cached tick yet it is subscribed and the first tick awaited (max ~3 s).
    async fn conversion_prices_subscribing(&self, symbol_name: &str, current_symbol: &str, fallback: f64) -> Option<(f64, f64)> {
        if let Some(p) = self.get_conversion_prices(symbol_name, current_symbol, fallback) { return Some(p); }
        let id = self.get_symbol_id(symbol_name)?;
        if let Err(e) = self.subscribe_ticks(&[id]).await { debug!("could not subscribe conversion symbol {symbol_name}: {e}"); return None; }
        for _ in 0..15 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if let Some(p) = self.get_conversion_prices(symbol_name, current_symbol, fallback) { return Some(p); }
        }
        None
    }

    fn get_conversion_prices(&self, symbol_name: &str, current_symbol: &str, fallback: f64) -> Option<(f64, f64)> {
        let (mut bid, mut ask) = self.st().tick_cache_by_name.get(symbol_name).map(|t| (t.get_f64("bid"), t.get_f64("ask"))).unwrap_or((0.0, 0.0));
        if symbol_name == current_symbol && fallback > 0.0 {
            if bid <= 0.0 { bid = fallback; }
            if ask <= 0.0 { ask = fallback; }
        }
        if bid <= 0.0 { bid = ask; }
        if ask <= 0.0 { ask = bid; }
        if bid > 0.0 && ask > 0.0 { Some((bid, ask)) } else { None }
    }

    /// Raw profit without currency conversion. `None` (and `last_error`) when inputs are unusable.
    pub(crate) fn calc_profit_raw(&self, info: &Record, is_buy: bool, volume: f64, price_open: f64, price_close: f64) -> Option<f64> {
        let mode = info.get_i64("trade_calc_mode");
        let contract_size = info.get_f64("contract_size");
        let tick_size = info.get_f64("tick_size");
        let tick_value = info.get_f64("tick_value");
        let direction = if is_buy { 1.0 } else { -1.0 };
        if BOND_CALC_MODES.contains(&mode) {
            let face_value = info.get_f64("face_value");
            let accrued = info.get_f64("accrued_interest");
            if contract_size <= 0.0 || face_value <= 0.0 {
                self.fail_last_error(-10, format!("trade_calc_mode={mode} requires contract_size and face_value for bond profit calculation"));
                return None;
            }
            let open_value = (price_open / 100.0) * face_value;
            let close_value = (price_close / 100.0) * face_value + accrued;
            return Some(direction * (close_value - open_value) * contract_size * volume);
        }
        if mode == COLLATERAL_CALC_MODE { return Some(0.0); }
        if FOREX_CALC_MODES.contains(&mode) || CFD_CALC_MODES.contains(&mode) {
            if contract_size <= 0.0 { self.fail_last_error(-11, "contract_size must be > 0 for profit calculation"); return None; }
            return Some(direction * (price_close - price_open) * contract_size * volume);
        }
        if matches!(mode, 1 | 33 | 35 | 36) {
            if tick_value <= 0.0 { self.fail_last_error(-12, "tick_value must be > 0 for futures/options profit calculation"); return None; }
            let point_value = if tick_size > 0.0 { tick_value / tick_size } else { tick_value };
            return Some(direction * (price_close - price_open) * volume * point_value);
        }
        if mode == 34 {
            if tick_value <= 0.0 { self.fail_last_error(-12, "tick_value must be > 0 for FORTS profit calculation"); return None; }
            let f = |p: f64| if tick_size > 0.0 { p * tick_value / tick_size } else { p * tick_value };
            return Some(direction * (f(price_close) - f(price_open)) * volume);
        }
        if contract_size <= 0.0 { self.fail_last_error(-11, "contract_size must be > 0 for profit calculation"); return None; }
        Some(direction * (price_close - price_open) * contract_size * volume)
    }

    /// Raw margin requirement without currency conversion.
    pub(crate) fn calc_margin_raw(&self, info: &Record, volume: f64, price: f64, leverage: i64) -> Option<f64> {
        let mode = info.get_i64("trade_calc_mode");
        let contract_size = info.get_f64("contract_size");
        let tick_size = info.get_f64("tick_size");
        let tick_value = info.get_f64("tick_value");
        let margin_initial = info.get_f64("margin_initial");
        let lots = volume;
        let units = lots * contract_size;
        let fail = |c: &Self, code: i64, msg: &str| -> Option<f64> { c.fail_last_error(code, msg); None };
        if BOND_CALC_MODES.contains(&mode) {
            let face_value = info.get_f64("face_value");
            if contract_size <= 0.0 || face_value <= 0.0 {
                return fail(self, -13, &format!("trade_calc_mode={mode} requires contract_size and face_value for bond margin calculation"));
            }
            return Some(lots * contract_size * face_value * price / 100.0);
        }
        if mode == COLLATERAL_CALC_MODE { return Some(0.0); }
        if mode == 5 {
            if contract_size <= 0.0 { return fail(self, -14, "contract_size must be > 0 for margin calculation"); }
            return Some(units);
        }
        if mode == 0 || mode == 4 {
            if contract_size <= 0.0 { return fail(self, -14, "contract_size must be > 0 for margin calculation"); }
            if leverage <= 0 { return fail(self, -15, "account leverage must be > 0 for leveraged margin calculation"); }
            let mut base = units / leverage as f64;
            if mode == 4 { base *= price; }
            return Some(base);
        }
        if FUTURES_CALC_MODES.contains(&mode) {
            if margin_initial > 0.0 { return Some(lots * margin_initial); }
            if tick_value <= 0.0 { return fail(self, -16, "margin_initial or tick_value is required for futures margin calculation"); }
            let pv = if tick_size > 0.0 { tick_value / tick_size } else { tick_value };
            return Some(lots * price * pv);
        }
        if mode == 3 {
            if contract_size <= 0.0 || tick_value <= 0.0 {
                return fail(self, -17, "contract_size and tick_value are required for CFD index margin calculation");
            }
            let pv = if tick_size > 0.0 { tick_value / tick_size } else { tick_value };
            return Some(units * price * pv);
        }
        if matches!(mode, 2 | 32 | 35 | 36 | 38) {
            if margin_initial > 0.0 && matches!(mode, 32 | 35 | 36 | 38) { return Some(lots * margin_initial); }
            if contract_size <= 0.0 { return fail(self, -14, "contract_size must be > 0 for margin calculation"); }
            return Some(units * price);
        }
        fail(self, -18, &format!("trade_calc_mode={mode} is not supported yet"))
    }

    /// Account margin figures computed locally from the live positions.
    ///
    /// The cmd=3 account record has no margin fields and the pushes do not carry used margin either - the Web Terminal
    /// computes it client-side - so this does the same: for every open position
    /// `margin = calc_margin_raw(symbol, lots, price_open, leverage) * rate_margin`, where `rate_margin` is the
    /// margin-currency -> account-currency rate the server supplies on each position (falls back to tick-based
    /// conversion), and floating profit is re-priced from the cached tick when available.
    ///
    /// Returns `margin`, `margin_free`, `margin_level`, `equity`, `profit` (floating), `positions`, `unpriced`
    /// (positions that could not be priced - their margin is missing) and `margin_source = "computed"`.
    /// Hedged volume (min of buy and sell lots per symbol) is charged `margin_hedged` instead of the full margin
    /// (`hedged_offset` is the amount saved). Implemented from the MT5 rules, **not yet checked against broker numbers**.
    pub async fn account_margin(&self) -> Result<Record> {
        let account = self.get_account().await?;
        self.account_margin_from(&account).await
    }

    pub(crate) async fn account_margin_from(&self, account: &Record) -> Result<Record> {
        let leverage = account.get_i64("leverage");
        let account_ccy = account.get_str("currency").to_string();
        let positions: RecordList = self.get_positions().await?;
        let (mut margin, mut floating, mut unpriced) = (0.0f64, 0.0f64, 0usize);
        // per symbol: (buy lots, buy margin, sell lots, sell margin, hedged cost per lot in account currency)
        let mut sides: std::collections::HashMap<String, (f64, f64, f64, f64, f64)> = std::collections::HashMap::new();
        let mut ordered: Vec<&Record> = positions.iter().collect();
        ordered.sort_by_key(|p| (p.get_i64("time_create"), p.get_u64("position_id")));
        let mut cumulative: std::collections::HashMap<(String, bool), f64> = std::collections::HashMap::new();
        for p in ordered {
            let symbol = p.get_str("trade_symbol").to_string();
            let lots = p.get_f64("trade_volume") / 10f64.powi(self.volume_precision());
            let is_buy = p.get_i64("trade_action") == POSITION_TYPE_BUY;
            let Some(info) = self.symbol_info(&symbol).await? else { unpriced += 1; floating += p.get_f64("profit"); continue };
            match self.calc_margin_raw(&info, lots, p.get_f64("price_open"), leverage) {
                Some(raw) => {
                    let prior = cumulative.entry((symbol.clone(), is_buy)).or_insert(0.0);
                    let raw = raw * margin_rate_factor(&info, account, *prior, lots, is_buy);
                    *prior += lots;
                    let mut rate = p.get_f64("rate_margin");
                    if rate <= 0.0 {
                        let ccy = { let c = info.get_str("currency_margin"); if c.is_empty() { account_ccy.clone() } else { c.to_string() } };
                        rate = self.resolve_conversion_rates(&ccy, &account_ccy, &symbol, p.get_f64("price_open")).await?.map(|(buy, _)| buy).unwrap_or(0.0);
                    }
                    if rate > 0.0 {
                        margin += raw * rate;
                        let e = sides.entry(symbol.clone()).or_default();
                        if is_buy { e.0 += lots; e.1 += raw * rate; } else { e.2 += lots; e.3 += raw * rate; }
                        // margin_hedged is in contract-size units for leveraged (calc mode 0) symbols; 0 = hedged volume is free
                        e.4 = if info.get_i64("trade_calc_mode") == 0 && leverage > 0 { info.get_f64("margin_hedged") / leverage as f64 * rate } else { 0.0 };
                    } else { unpriced += 1; }
                }
                None => unpriced += 1,
            }
            // floating profit: re-price with the cached tick if possible, otherwise use the last reported value
            let rate_profit = p.get_f64("rate_profit");
            let repriced = self.symbol_info_tick(&symbol).and_then(|t| {
                let now = if is_buy { t.get_f64("bid") } else { t.get_f64("ask") };
                if now > 0.0 && rate_profit > 0.0 { self.calc_profit_raw(&info, is_buy, lots, p.get_f64("price_open"), now).map(|raw| raw * rate_profit) } else { None }
            });
            floating += repriced.unwrap_or_else(|| p.get_f64("profit"));
        }
        let mut hedged_offset = 0.0;
        for (bl, bm, sl, sm, cost) in sides.values() {
            let adjusted = hedged_margin(*bl, *bm, *sl, *sm, *cost);
            hedged_offset += bm + sm - adjusted;
        }
        margin -= hedged_offset;
        let equity = account.get_f64("balance") + account.get_f64("credit") + floating;
        Ok(Record::new()
            .with("hedged_offset", hedged_offset)
            .with("margin", margin)
            .with("equity", equity)
            .with("profit", floating)
            .with("margin_free", equity - margin)
            .with("margin_level", if margin > 0.0 { equity / margin * 100.0 } else { 0.0 })
            .with("positions", positions.len())
            .with("unpriced", unpriced)
            .with("margin_source", "computed"))
    }
}


fn trade_part(info: &Record) -> &Record { info.get_record("trade").unwrap_or(info) }

fn path_matches(patterns: &str, path: &str) -> bool {
    patterns.split(',').map(str::trim).filter(|p| !p.is_empty()).any(|p| match p.strip_suffix('*') { Some(prefix) => path.starts_with(prefix), None => p == path })
}

/// Multiplier on the leveraged margin that the server's margin rules imply (verified against the Web Terminal:
/// 0.01 BTCEURs = 40.37 USD = notional * `margin_rates_initial` 0.10 * leverage-rule tier 0.5).
///
/// * the symbol's `margin_rates_initial[0|1]` (buy|sell); non-positive values count as 1;
/// * the first account `leverage_rules` entry whose `path` list matches `trade.symbol_path` (`*` suffix = prefix match):
///   tiers are ranges of the symbol's **cumulative** volume in lots (`range_mode` 1, positions in opening order);
///   each tier's `margin_rate_initial` applies to the part of the volume that falls inside it (verified: 0.01 + 2.00 lot
///   BTCEURs = 12175.61 in the Web Terminal). `prior_lots` = volume of the same symbol/side opened before.
pub(crate) fn margin_rate_factor(info: &Record, account: &Record, prior_lots: f64, lots: f64, is_buy: bool) -> f64 {
    let trade = trade_part(info);
    let rate = trade.get_list("margin_rates_initial").get(if is_buy { 0 } else { 1 }).and_then(Value::as_f64).filter(|r| *r > 0.0).unwrap_or(1.0);
    let path = trade.get_str("symbol_path");
    let tier_factor = (|| {
        if lots <= 0.0 || path.is_empty() { return None; }
        let rule = account.get_records("leverage_rules").into_iter().find(|r| path_matches(r.get_str("path"), path))?;
        if rule.get_i64("range_mode") != 1 { return None; }
        let mut total = 0.0;
        for t in rule.get_records("tiers") {
            let (from, to) = (t.get_f64("range_from"), t.get_f64("range_to"));
            // the volume of this order occupies [prior_lots, prior_lots + lots) of the symbol's cumulative volume
            let part = ((prior_lots + lots).min(to) - prior_lots.max(from)).max(0.0);
            total += part * t.get_f64("margin_rate_initial");
        }
        (total > 0.0).then(|| total / lots)
    })();
    rate * tier_factor.unwrap_or(1.0)
}

/// Margin of one symbol's positions after hedging: the overlapping volume `h = min(buy, sell)` is charged
/// `h * hedged_cost_per_lot` instead of `h` lots at the average margin of each side.
pub(crate) fn hedged_margin(buy_lots: f64, buy_margin: f64, sell_lots: f64, sell_margin: f64, hedged_cost_per_lot: f64) -> f64 {
    if buy_lots <= 0.0 || sell_lots <= 0.0 { return buy_margin + sell_margin; }
    let h = buy_lots.min(sell_lots);
    buy_margin + sell_margin - h * buy_margin / buy_lots - h * sell_margin / sell_lots + h * hedged_cost_per_lot
}

#[cfg(test)]
mod tests {
    use super::{hedged_margin, margin_rate_factor, split_cent_currency};
    use crate::protocol::{Record, Value};

    fn list(v: &[f64]) -> Value { Value::List(v.iter().map(|x| Value::Float(*x)).collect()) }
    fn tier(from: f64, to: f64, rate: f64) -> Value { Value::Record(Record::new().with("range_from", from).with("range_to", to).with("margin_rate_initial", rate)) }

    #[test]
    fn margin_rules_match_web_terminal_btceur() {
        let mut trade = Record::new().with("symbol_path", "C3\\Cryptos\\BTCEURs");
        trade.set("margin_rates_initial", list(&[0.1, 0.1, 0.0]));
        let mut info = Record::new(); info.set("trade", trade);
        let rule = |path: &str, rate: f64| Value::Record({
            let mut r = Record::new().with("path", path).with("range_mode", 1u64);
            r.set("tiers", Value::List(vec![tier(0.0, 1.0, rate), tier(1.0, f64::MAX, 1.0)]));
            r
        });
        let mut account = Record::new();
        account.set("leverage_rules", Value::List(vec![rule("C3\\Cryptos\\BTCUSDs", 0.04), rule("C3\\CryptosP\\*,C3\\Cryptos\\*", 0.5)]));
        // 0.01 lot at 71700.84 EUR * 1.12595 USD/EUR: the terminal showed 40.37
        let raw = 0.01 * 71700.84 * 1.12595;
        assert!((raw * margin_rate_factor(&info, &account, 0.0, 0.01, true) - 40.37).abs() < 0.01);
        // 2 lots: first lot at 0.5, second at 1.0 -> average 0.75, times the symbol rate 0.1
        assert!((margin_rate_factor(&info, &account, 0.0, 2.0, true) - 0.075).abs() < 1e-12);
        // 2.00 lots opened after 0.01: 0.99 lot at 0.5 and 1.01 lot at 1.0 (terminal: 12175.61 in total)
        let f = margin_rate_factor(&info, &account, 0.01, 2.0, true);
        let total = 0.1 * 0.01 * 0.5 * 71700.84 * 1.12595 + f * 2.0 * 71596.47 * 1.12621;
        assert!((total - 12175.61).abs() < 0.1, "{total}");
        // no matching rule / no data -> neutral
        assert_eq!(margin_rate_factor(&Record::new(), &Record::new(), 0.0, 1.0, true), 1.0);
    }
    #[test]
    fn hedged_margin_offsets_overlap() {
        // 0.3 buy (3.0) vs 0.1 sell (1.0): only the net 0.2 lot is charged when margin_hedged = 0
        assert!((hedged_margin(0.3, 3.0, 0.1, 1.0, 0.0) - 2.0).abs() < 1e-12);
        // fully hedged and free
        assert!(hedged_margin(0.1, 1.0, 0.1, 1.0, 0.0).abs() < 1e-12);
        // a hedged cost of 0.5/lot adds back 0.1 * 0.5
        assert!((hedged_margin(0.1, 1.0, 0.1, 1.0, 0.5) - 0.05).abs() < 1e-12);
        // one-sided positions are untouched
        assert_eq!(hedged_margin(0.3, 3.0, 0.0, 0.0, 0.0), 3.0);
    }
    #[test]
    fn cent_currencies() {
        assert_eq!(split_cent_currency("USC"), ("USD", 100.0));
        assert_eq!(split_cent_currency("EUR"), ("EUR", 1.0));
        assert_eq!(split_cent_currency("EUC"), ("EUR", 100.0));
    }
}
