//! Currency conversion, profit and margin formulas (port of `pymt5._currency`).

use tracing::debug;

use crate::client::{is_soft_error, Mt5Client};
use crate::constants::*;
use crate::error::Result;
use crate::parsers::currencies_equal;
use crate::protocol::Record;

impl Mt5Client {
    /// Buy/sell conversion rates `source -> target`; tries direct, then triangulates via USD.
    pub(crate) async fn resolve_conversion_rates(&self, source: &str, target: &str, current_symbol: &str, fallback: f64) -> Result<Option<(f64, f64)>> {
        if source.is_empty() || target.is_empty() { return Ok(None); }
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
            if let Some((bid, ask)) = self.get_conversion_prices(&direct, current_symbol, fallback) {
                return Ok(if prefer_ask_when_direct { ask } else { bid });
            }
        }
        if let Some(inverse) = self.find_conversion_symbol_name(target, source).await? {
            if let Some((bid, ask)) = self.get_conversion_prices(&inverse, current_symbol, fallback) {
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
        Ok(matches.first().map(|s| (*s).clone()))
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
}
