//! Market data: symbols, ticks, bars, order book (port of `pymt5._market_data`).

use std::collections::HashMap;
use std::io::Read;
use std::time::{Instant, SystemTime};

use tracing::{debug, info, warn};

use crate::client::{trunc, Mt5Client};
use crate::constants::*;
use crate::error::{Error, Result};
use crate::parsers::*;
use crate::protocol::{get_series_size, Record, RecordList, SerField, SeriesCodec};
use crate::schemas::*;
use crate::subscription::{SubscriptionHandle, SubscriptionKind};
use crate::types::SymbolInfo;
use crate::validation::validate_symbol_name;

fn id_list_payload(ids: &[u32]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + ids.len() * 4);
    p.extend((ids.len() as u32).to_le_bytes());
    for id in ids { p.extend(id.to_le_bytes()); }
    p
}

impl Mt5Client {
    fn is_symbol_cache_valid(&self) -> bool {
        let ttl = self.config().symbol_cache_ttl;
        if ttl <= 0.0 { return false; }
        self.st().symbols_loaded_at.is_some_and(|t| t.elapsed().as_secs_f64() < ttl)
    }

    /// Force the next [`Mt5Client::load_symbols`] to refetch.
    pub fn invalidate_symbol_cache(&self) { self.st().symbols_loaded_at = None; }

    /// Load symbols and build the name/id caches (honours `symbol_cache_ttl`).
    pub async fn load_symbols(&self, use_gzip: bool) -> Result<HashMap<String, SymbolInfo>> {
        if !self.st().symbols.is_empty() && self.is_symbol_cache_valid() {
            debug!("symbol cache still valid, skipping reload");
            return Ok(self.st().symbols.clone());
        }
        let raw = self.get_symbols(use_gzip).await?;
        let mut st = self.st();
        st.symbols.clear();
        st.symbols_by_id.clear();
        st.full_symbols.clear();
        for s in &raw {
            let info = SymbolInfo {
                name: s.get_str("trade_symbol").to_string(),
                symbol_id: s.get_u64("symbol_id") as u32,
                digits: s.get_u64("digits") as u32,
                description: s.get_str("symbol_description").to_string(),
                path: s.get_str("symbol_path").to_string(),
                trade_calc_mode: s.get_i64("trade_calc_mode"),
                basis: s.get_str("basis").to_string(),
                sector: s.get_i64("sector"),
            };
            st.symbols_by_id.insert(info.symbol_id, info.clone());
            st.symbols.insert(info.name.clone(), info);
        }
        st.symbols_loaded_at = Some(Instant::now());
        info!("symbol cache loaded: {} symbols", st.symbols.len());
        Ok(st.symbols.clone())
    }

    pub fn get_symbol_info(&self, name: &str) -> Option<SymbolInfo> { self.st().symbols.get(name).cloned() }
    pub fn get_symbol_id(&self, name: &str) -> Option<u32> { self.st().symbols.get(name).map(|s| s.symbol_id) }
    pub fn symbol_names(&self) -> Vec<String> { self.st().symbols.keys().cloned().collect() }

    pub(crate) async fn resolve_symbol_id(&self, symbol: &str) -> Result<Option<u32>> {
        if let Some(id) = self.get_symbol_id(symbol) { return Ok(Some(id)); }
        self.load_symbols(true).await?;
        Ok(self.get_symbol_id(symbol))
    }

    /// Raw symbol records (cmd=34 gzip/zlib or cmd=6).
    pub async fn get_symbols(&self, use_gzip: bool) -> Result<RecordList> {
        if use_gzip {
            let r = self.transport().send_command(CMD_GET_SYMBOLS_GZIP, &[]).await?;
            if r.body.len() > 4 {
                let compressed = r.body[4..].to_vec();
                let raw = tokio::task::spawn_blocking(move || {
                    let mut out = Vec::new();
                    flate2::read::ZlibDecoder::new(&compressed[..]).read_to_end(&mut out).map(|_| out)
                })
                .await
                .map_err(|e| Error::protocol(format!("decompress task failed: {e}")))?
                .map_err(|e| Error::protocol(format!("zlib decompress failed: {e}")))?;
                return Ok(parse_counted_records(&raw, SYMBOL_BASIC_SCHEMA, SYMBOL_BASIC_FIELD_NAMES));
            }
            return Ok(vec![]);
        }
        let r = self.transport().send_command(CMD_GET_SYMBOLS, &[]).await?;
        Ok(parse_counted_records(&r.body, SYMBOL_BASIC_SCHEMA, SYMBOL_BASIC_FIELD_NAMES))
    }

    pub async fn symbols_total(&self, use_gzip: bool) -> Result<usize> {
        let n = self.st().symbols.len();
        if n > 0 { return Ok(n); }
        Ok(self.get_symbols(use_gzip).await?.len())
    }

    /// Symbol list with optional wildcard group filter.
    pub async fn symbols_get(&self, group: Option<&str>, use_gzip: bool) -> Result<RecordList> {
        let symbols = self.get_symbols(use_gzip).await?;
        Ok(match group {
            None | Some("") => symbols,
            Some(g) => symbols.into_iter().filter(|s| matches_group_mask(s.get_str("trade_symbol"), g)).collect(),
        })
    }

    /// Detailed contract specs for several symbol ids (cmd=18).
    ///
    /// The Web Terminal bundle sends `[u32 count][u32 symbol_id]*` and receives `[u32 count][record]*`.
    pub async fn get_full_symbols(&self, symbol_ids: &[u32]) -> Result<RecordList> {
        if symbol_ids.is_empty() { return Ok(vec![]); }
        let r = self.transport().send_command(CMD_GET_FULL_SYMBOLS, &id_list_payload(symbol_ids)).await?;
        if r.code != 0 || r.body.is_empty() { return Ok(vec![]); }
        Ok(parse_counted_records(&r.body, FULL_SYMBOL_SCHEMA, FULL_SYMBOL_FIELD_NAMES).iter().map(normalize_full_symbol_record).collect())
    }

    /// Detailed contract spec (cmd=18). `None` when the symbol is unknown/unparseable.
    ///
    /// Uses the symbol-id request format of the current Web Terminal (loading the symbol list first
    /// if needed). pymt5 sends the symbol *name* instead, which servers such as build 6182 reject
    /// with `code=1`; that legacy form is kept as a fallback for older servers.
    pub async fn get_full_symbol_info(&self, symbol: &str) -> Result<Option<Record>> {
        validate_symbol_name(symbol)?;
        if let Some(id) = self.resolve_symbol_id(symbol).await? {
            if let Some(info) = self.get_full_symbols(&[id]).await?.into_iter().next() {
                self.st().full_symbols.insert(symbol.to_string(), info.clone());
                return Ok(Some(info));
            }
        }
        let payload = SeriesCodec::serialize(&[SerField::fixed_string(&trunc(symbol, 32), 64)])?;
        let r = self.transport().send_command(CMD_GET_FULL_SYMBOLS, &payload).await?;
        if r.code != 0 || r.body.is_empty() { return Ok(None); }
        let records = parse_counted_records(&r.body, FULL_SYMBOL_SCHEMA, FULL_SYMBOL_FIELD_NAMES);
        let parsed: Result<Option<Record>> = (|| {
            if let Some(first) = records.first() {
                return Ok(Some(normalize_full_symbol_record(first)));
            }
            if r.body.len() >= get_series_size(FULL_SYMBOL_SCHEMA)? {
                let rec = SeriesCodec::parse_record(&r.body, FULL_SYMBOL_SCHEMA, FULL_SYMBOL_FIELD_NAMES, 0)?;
                return Ok(Some(normalize_full_symbol_record(&rec)));
            }
            Ok(None)
        })();
        match parsed {
            Ok(Some(info)) => {
                self.st().full_symbols.insert(symbol.to_string(), info.clone());
                Ok(Some(info))
            }
            Ok(None) => Ok(None),
            Err(e) => { warn!("get_full_symbol_info({symbol}) failed: {e}"); Ok(None) }
        }
    }

    /// Official-style `symbol_info()` (full spec, falling back to the basic symbol record).
    pub async fn symbol_info(&self, symbol: &str) -> Result<Option<Record>> {
        if let Some(c) = self.st().full_symbols.get(symbol) { return Ok(Some(c.clone())); }
        if let Some(i) = self.get_full_symbol_info(symbol).await? { return Ok(Some(i)); }
        if self.st().symbols.is_empty() { self.load_symbols(true).await?; }
        Ok(self.get_symbol_info(symbol).map(|b| {
            Record::new()
                .with("trade_symbol", b.name)
                .with("symbol_id", b.symbol_id)
                .with("digits", b.digits)
                .with("symbol_description", b.description)
                .with("symbol_path", b.path)
                .with("trade_calc_mode", b.trade_calc_mode)
                .with("basis", b.basis)
                .with("sector", b.sector)
        }))
    }

    /// Latest cached tick (needs an active tick subscription).
    pub fn symbol_info_tick(&self, symbol: &str) -> Option<Record> { self.st().tick_cache_by_name.get(symbol).cloned() }

    /// Best-effort `symbol_select` via tick subscription.
    pub async fn symbol_select(&self, symbol: &str, enable: bool) -> Result<bool> {
        let Some(id) = self.resolve_symbol_id(symbol).await? else { return Ok(false) };
        if enable {
            self.subscribe_ticks(&[id]).await?;
        } else {
            self.unsubscribe_ticks(&[id]).await?;
            let mut st = self.st();
            st.tick_cache_by_id.remove(&id);
            st.tick_cache_by_name.remove(symbol);
        }
        Ok(true)
    }

    /// Symbol group names (cmd=9).
    pub async fn get_symbol_groups(&self) -> Result<Vec<String>> {
        let r = self.transport().send_command(CMD_GET_SYMBOL_GROUPS, &[]).await?;
        if r.body.len() < 4 { return Ok(vec![]); }
        let count = u32::from_le_bytes(r.body[0..4].try_into().unwrap());
        let size = get_series_size(SYMBOL_GROUP_SCHEMA)?;
        let mut groups = Vec::new();
        let mut offset = 4;
        for _ in 0..count {
            if offset + size > r.body.len() { break; }
            let v = SeriesCodec::parse(&r.body, SYMBOL_GROUP_SCHEMA, offset)?;
            groups.push(v[0].as_str().unwrap_or("").to_string());
            offset += size;
        }
        info!("loaded {} symbol groups", groups.len());
        Ok(groups)
    }

    /// Spread data (cmd=20); empty `symbol_ids` sends an empty payload.
    pub async fn get_spreads(&self, symbol_ids: &[u32]) -> Result<RecordList> {
        let payload = if symbol_ids.is_empty() { vec![] } else { id_list_payload(symbol_ids) };
        let r = self.transport().send_command(CMD_GET_SPREADS, &payload).await?;
        Ok(parse_counted_records(&r.body, SPREAD_SCHEMA, SPREAD_FIELD_NAMES))
    }

    /// Subscribe to ticks. The server replaces its set, so the full merged set is always sent.
    pub async fn subscribe_ticks(&self, symbol_ids: &[u32]) -> Result<()> {
        if symbol_ids.is_empty() {
            debug!("subscribe_ticks called with empty list, skipping");
            return Ok(());
        }
        let mut merged: Vec<u32> = self.st().subscribed_ids.iter().chain(symbol_ids).copied().collect();
        merged.sort_unstable();
        merged.dedup();
        self.transport().send_command(CMD_SUBSCRIBE_TICKS, &id_list_payload(&merged)).await?;
        info!("subscribed to {} symbol ids for ticks", merged.len());
        self.st().subscribed_ids = merged;
        Ok(())
    }

    pub async fn unsubscribe_ticks(&self, symbol_ids: &[u32]) -> Result<()> {
        let remaining: Vec<u32> = self.st().subscribed_ids.iter().copied().filter(|i| !symbol_ids.contains(i)).collect();
        self.transport().send_command(CMD_SUBSCRIBE_TICKS, &id_list_payload(&remaining)).await?;
        info!("unsubscribed ticks ({} remaining)", remaining.len());
        self.st().subscribed_ids = remaining;
        Ok(())
    }

    fn names_to_ids(&self, names: &[&str]) -> std::result::Result<Vec<u32>, Vec<String>> {
        let st = self.st();
        let mut ids = Vec::new();
        let mut missing = Vec::new();
        for n in names {
            match st.symbols.get(*n) { Some(i) => ids.push(i.symbol_id), None => missing.push(n.to_string()) }
        }
        if missing.is_empty() { Ok(ids) } else { Err(missing) }
    }

    /// Subscribe by symbol name (call `load_symbols` first). Errors if any name is unknown.
    pub async fn subscribe_symbols(&self, names: &[&str]) -> Result<Vec<u32>> {
        let ids = self.names_to_ids(names).map_err(|m| Error::SymbolNotFound(format!("symbols not found in cache (call load_symbols first): {m:?}")))?;
        self.subscribe_ticks(&ids).await?;
        Ok(ids)
    }

    /// Like [`Mt5Client::subscribe_symbols`] but silently skips unknown names.
    pub async fn subscribe_symbols_batch(&self, names: &[&str]) -> Result<Vec<u32>> {
        let ids: Vec<u32> = { let st = self.st(); names.iter().filter_map(|n| st.symbols.get(*n).map(|s| s.symbol_id)).collect() };
        if !ids.is_empty() { self.subscribe_ticks(&ids).await?; }
        Ok(ids)
    }

    /// Subscribe to the order book / DOM (cmd=22); pushes arrive as cmd=23.
    pub async fn subscribe_book(&self, symbol_ids: &[u32]) -> Result<()> {
        let mut merged: Vec<u32> = self.st().subscribed_book_ids.iter().chain(symbol_ids).copied().collect();
        merged.sort_unstable();
        merged.dedup();
        self.transport().send_command(CMD_SUBSCRIBE_BOOK, &id_list_payload(&merged)).await?;
        info!("subscribed to order book for {} symbols", merged.len());
        self.st().subscribed_book_ids = merged;
        Ok(())
    }

    pub async fn subscribe_book_by_name(&self, names: &[&str]) -> Result<Vec<u32>> {
        let ids = self.names_to_ids(names).map_err(|m| Error::SymbolNotFound(format!("symbols not found in cache (call load_symbols first): {m:?}")))?;
        self.subscribe_book(&ids).await?;
        Ok(ids)
    }

    pub async fn unsubscribe_book(&self, symbol_ids: &[u32]) -> Result<()> {
        let existing = self.st().subscribed_book_ids.clone();
        let remaining: Vec<u32> = existing.iter().copied().filter(|i| !symbol_ids.contains(i)).collect();
        self.transport().send_command(CMD_SUBSCRIBE_BOOK, &id_list_payload(&remaining)).await?;
        let mut st = self.st();
        for id in existing.iter().filter(|i| !remaining.contains(i)) {
            if let Some(e) = st.book_cache_by_id.remove(id) {
                let name = e.get_str("symbol");
                if !name.is_empty() { st.book_cache_by_name.remove(name); }
            }
        }
        st.subscribed_book_ids = remaining;
        Ok(())
    }

    /// Subscribe to ticks and get a handle to unsubscribe with.
    pub async fn subscribe_ticks_managed(&self, symbol_ids: &[u32]) -> Result<SubscriptionHandle> {
        self.subscribe_ticks(symbol_ids).await?;
        Ok(SubscriptionHandle::new(self.clone(), SubscriptionKind::Ticks, symbol_ids))
    }

    pub async fn subscribe_book_managed(&self, symbol_ids: &[u32]) -> Result<SubscriptionHandle> {
        self.subscribe_book(symbol_ids).await?;
        Ok(SubscriptionHandle::new(self.clone(), SubscriptionKind::Book, symbol_ids))
    }

    pub async fn market_book_add(&self, symbol: &str) -> Result<bool> {
        let Some(id) = self.resolve_symbol_id(symbol).await? else { return Ok(false) };
        self.subscribe_book(&[id]).await?;
        Ok(true)
    }

    pub fn market_book_get(&self, symbol: &str) -> Option<Record> { self.st().book_cache_by_name.get(symbol).cloned() }

    pub async fn market_book_release(&self, symbol: &str) -> Result<bool> {
        let Some(id) = self.resolve_symbol_id(symbol).await? else { return Ok(false) };
        self.unsubscribe_book(&[id]).await?;
        let mut st = self.st();
        st.book_cache_by_name.remove(symbol);
        st.book_cache_by_id.remove(&id);
        Ok(true)
    }

    /// Tick-history memory usage: `symbols_tracked`, `total_ticks`, `limit_per_symbol`.
    pub fn tick_history_stats(&self) -> Record {
        let st = self.st();
        let tracked: Vec<_> = st.tick_history_by_id.iter().filter(|(id, _)| st.symbols_by_id.contains_key(id)).collect();
        Record::new()
            .with("symbols_tracked", tracked.len())
            .with("total_ticks", tracked.iter().map(|(_, h)| h.len()).sum::<usize>())
            .with("limit_per_symbol", self.config().tick_history_limit)
    }

    fn rates_payload(symbol: &str, period_minutes: i64, from_ts: i64, to_ts: i64) -> Result<Vec<u8>> {
        let mapped = period_map().get(&period_minutes).map(|c| *c as i64).unwrap_or(period_minutes);
        SeriesCodec::serialize(&[
            SerField::fixed_string(&trunc(symbol, 32), 64),
            SerField::new(crate::protocol::PropType::U16, mapped, None),
            SerField::new(crate::protocol::PropType::I32, from_ts, None),
            SerField::new(crate::protocol::PropType::I32, to_ts, None),
        ])
    }

    /// OHLCV bars (cmd=11). `period_minutes` is e.g. 1, 5, 15, 30, 60, 240, 1440, 10080, 43200.
    pub async fn get_rates(&self, symbol: &str, period_minutes: i64, from_ts: i64, to_ts: i64) -> Result<RecordList> {
        let payload = Self::rates_payload(symbol, period_minutes, from_ts, to_ts)?;
        let r = self.transport().send_command(CMD_GET_RATES, &payload).await?;
        Ok(parse_rate_bars(&r.body))
    }

    /// Raw rate bytes (debugging).
    pub async fn get_rates_raw(&self, symbol: &str, period_minutes: i64, from_ts: i64, to_ts: i64) -> Result<Vec<u8>> {
        let payload = Self::rates_payload(symbol, period_minutes, from_ts, to_ts)?;
        Ok(self.transport().send_command(CMD_GET_RATES, &payload).await?.body)
    }

    pub async fn copy_rates_range(&self, symbol: &str, timeframe: i64, date_from: impl Into<TimeArg>, date_to: impl Into<TimeArg>) -> Result<RecordList> {
        let pm = normalize_timeframe_minutes(timeframe);
        self.get_rates(symbol, pm, coerce_timestamp(date_from.into()), coerce_timestamp(date_to.into())).await
    }

    /// Up to `count` bars ending at `date_from`.
    pub async fn copy_rates_from(&self, symbol: &str, timeframe: i64, date_from: impl Into<TimeArg>, count: i64) -> Result<RecordList> {
        if count <= 0 { return Ok(vec![]); }
        let pm = normalize_timeframe_minutes(timeframe);
        let end = coerce_timestamp(date_from.into());
        let lookback = history_lookback_seconds(pm, count);
        let bars = self.get_rates(symbol, pm, (end - lookback).max(0), end).await?;
        let skip = bars.len().saturating_sub(count as usize);
        Ok(bars[skip..].to_vec())
    }

    /// `count` bars counted back from the current bar (`start_pos = 0` is the latest).
    pub async fn copy_rates_from_pos(&self, symbol: &str, timeframe: i64, start_pos: i64, count: i64) -> Result<RecordList> {
        if start_pos < 0 { return Err(Error::validation(format!("start_pos must be >= 0, got {start_pos}"))); }
        if count <= 0 { return Ok(vec![]); }
        let pm = normalize_timeframe_minutes(timeframe);
        let end = coerce_timestamp(SystemTime::now().into());
        let lookback = history_lookback_seconds(pm, start_pos + count);
        let bars = self.get_rates(symbol, pm, (end - lookback).max(0), end).await?;
        if bars.is_empty() { return Ok(vec![]); }
        let end_index = (bars.len() as i64 - start_pos).max(0);
        let start_index = (end_index - count).max(0);
        Ok(bars[start_index as usize..end_index as usize].to_vec())
    }

    async fn get_tick_history(&self, symbol: &str) -> Result<Vec<Record>> {
        let Some(id) = self.resolve_symbol_id(symbol).await? else { return Ok(vec![]) };
        Ok(self.st().tick_history_by_id.get(&id).map(|h| h.iter().cloned().collect()).unwrap_or_default())
    }

    /// Cached tick history view (built from cmd=8 pushes) from `date_from`, up to `count`.
    pub async fn copy_ticks_from(&self, symbol: &str, date_from: impl Into<TimeArg>, count: i64, flags: u32) -> Result<RecordList> {
        if count <= 0 { return Ok(vec![]); }
        let history = self.get_tick_history(symbol).await?;
        let from_ms = coerce_timestamp_ms(date_from.into());
        Ok(history
            .iter()
            .filter(|t| t.get_i64("tick_time_ms") >= from_ms && tick_matches_copy_flags(t, flags))
            .map(to_copy_tick_record)
            .take(count as usize)
            .collect())
    }

    pub async fn copy_ticks_range(&self, symbol: &str, date_from: impl Into<TimeArg>, date_to: impl Into<TimeArg>, flags: u32) -> Result<RecordList> {
        let history = self.get_tick_history(symbol).await?;
        let from_ms = coerce_timestamp_ms(date_from.into());
        let to_ms = coerce_timestamp_ms_end(date_to.into());
        if to_ms < from_ms { return Ok(vec![]); }
        Ok(history
            .iter()
            .filter(|t| { let ms = t.get_i64("tick_time_ms"); from_ms <= ms && ms <= to_ms && tick_matches_copy_flags(t, flags) })
            .map(to_copy_tick_record)
            .collect())
    }
}
