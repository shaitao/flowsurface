use crate::{
    Kline, OpenInterest, Price, PushFrequency, Ticker, TickerInfo, TickerStats, Timeframe, Trade,
    Volume,
    adapter::{AdapterError, Event, StreamKind, StreamTicksize, Venue, flush_trade_buffers},
    connect::{State, channel, connect_ws},
    depth::{DeOrder, DepthPayload, DepthUpdate, LocalDepthCache},
    unit::Qty,
};

use chrono::{Datelike, Days, FixedOffset, NaiveDate, TimeZone};
use fastwebsockets::OpCode;
use futures::{SinkExt, Stream};
use indexmap::IndexMap;
use rustc_hash::FxHashMap;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    env,
    sync::{LazyLock, RwLock},
    time::{Duration, Instant},
};
use url::Url;

const DEFAULT_QMT_BRIDGE_BASE: &str = "http://127.0.0.1:8765";
const DEFAULT_QMT_INITIAL_KLINE_BARS: u64 = 450;
const QMT_VOLUME_LOT_SIZE: f32 = 100.0;
const QMT_KLINE_SEED_CALENDAR_LOOKBACK_MS: u64 = 14 * 86_400_000;
const QMT_CLOSE_BUCKET_GRACE_MS: u64 = 5_000;
const QMT_TICK_CACHE_MAX_DAYS_PER_SYMBOL: usize = 32;
const QMT_TICK_FETCH_FAILURE_COOLDOWN: Duration = Duration::from_secs(60);
static TRADING_DAY_CACHE: LazyLock<RwLock<HashMap<Venue, TradingDayCache>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static TICK_DAY_CACHE: LazyLock<RwLock<QmtTickDayCache>> =
    LazyLock::new(|| RwLock::new(QmtTickDayCache::new(QMT_TICK_CACHE_MAX_DAYS_PER_SYMBOL)));
static CURRENT_DAY_TICK_CACHE: LazyLock<RwLock<FxHashMap<Ticker, CurrentDayTickCacheEntry>>> =
    LazyLock::new(|| RwLock::new(FxHashMap::default()));
static TICK_FETCH_FAILURE_CACHE: LazyLock<RwLock<FxHashMap<(Ticker, NaiveDate), TickFetchFailureEntry>>> =
    LazyLock::new(|| RwLock::new(FxHashMap::default()));

fn qmt_bridge_base() -> String {
    env::var("QMT_BRIDGE_BASE").unwrap_or_else(|_| DEFAULT_QMT_BRIDGE_BASE.to_string())
}

fn qmt_bridge_url(
    path: &str,
    query_pairs: &[(&str, String)],
    websocket: bool,
) -> Result<(String, String), AdapterError> {
    let base = qmt_bridge_base();
    let mut url = Url::parse(&base).map_err(|e| AdapterError::InvalidRequest(e.to_string()))?;

    if websocket {
        let scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            other => {
                return Err(AdapterError::InvalidRequest(format!(
                    "Unsupported QMT bridge scheme: {other}"
                )));
            }
        };
        url.set_scheme(scheme).map_err(|_| {
            AdapterError::InvalidRequest("Failed to set websocket scheme".to_string())
        })?;
    }

    url.set_path(path);
    url.set_query(None);
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query_pairs {
            pairs.append_pair(key, value);
        }
    }

    let host = url
        .host_str()
        .ok_or_else(|| AdapterError::InvalidRequest("QMT bridge host missing".to_string()))?
        .to_string();

    Ok((host, url.into()))
}

fn qmt_bridge_http_url(path: &str, query_pairs: &[(&str, String)]) -> Result<String, AdapterError> {
    let (_, url) = qmt_bridge_url(path, query_pairs, false)?;
    Ok(url)
}

fn qmt_bridge_ws_url(
    path: &str,
    query_pairs: &[(&str, String)],
) -> Result<(String, String), AdapterError> {
    qmt_bridge_url(path, query_pairs, true)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QmtTick {
    time: u64,
    #[serde(default)]
    last_price: Option<f32>,
    #[serde(default)]
    open: Option<f32>,
    #[serde(default)]
    high: Option<f32>,
    #[serde(default)]
    low: Option<f32>,
    #[serde(default)]
    last_close: Option<f32>,
    #[serde(default)]
    amount: Option<f32>,
    #[serde(default)]
    volume: Option<u64>,
    #[serde(default)]
    transaction_num: Option<u64>,
    #[serde(default)]
    ask_price: Vec<f32>,
    #[serde(default)]
    bid_price: Vec<f32>,
    #[serde(default)]
    ask_vol: Vec<f32>,
    #[serde(default)]
    bid_vol: Vec<f32>,
}

#[derive(Debug, Deserialize)]
struct BridgeStatusMessage {
    phase: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum BridgeWsMessage {
    #[serde(rename = "tick")]
    Tick(QmtTick),
    #[serde(rename = "status")]
    Status(BridgeStatusMessage),
}

#[derive(Debug, Deserialize)]
struct BridgeItemsResponse<T> {
    items: Vec<T>,
}

#[derive(Debug)]
struct QmtTickDayCache {
    max_days_per_symbol: usize,
    entries: FxHashMap<Ticker, IndexMap<NaiveDate, Vec<QmtTick>>>,
}

#[derive(Debug)]
struct CurrentDayTickCacheEntry {
    day: NaiveDate,
    ticks: Vec<QmtTick>,
    history_loaded: bool,
}

#[derive(Debug, Clone)]
struct TickFetchFailureEntry {
    failed_at: Instant,
    error: String,
}

#[derive(Debug, Clone, Copy)]
struct LiveKlineStream {
    ticker_info: TickerInfo,
    timeframe: Timeframe,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeSearchItem {
    symbol: String,
    #[serde(default)]
    display_name: Option<String>,
    min_ticksize: f32,
    min_qty: f32,
}

#[derive(Default)]
struct SyntheticTradeState {
    previous_tick: Option<QmtTick>,
}

#[derive(Debug, Default)]
struct TradingDayCache {
    covered_ranges: Vec<(NaiveDate, NaiveDate)>,
    trading_days: HashSet<NaiveDate>,
}

impl QmtTickDayCache {
    fn new(max_days_per_symbol: usize) -> Self {
        Self {
            max_days_per_symbol,
            entries: FxHashMap::default(),
        }
    }

    fn get(&mut self, ticker: Ticker, day: NaiveDate) -> Option<Vec<QmtTick>> {
        let day_cache = self.entries.get_mut(&ticker)?;
        let ticks = day_cache.shift_remove(&day)?;
        let cached = ticks.clone();
        day_cache.insert(day, ticks);
        Some(cached)
    }

    fn insert(&mut self, ticker: Ticker, day: NaiveDate, ticks: Vec<QmtTick>) {
        if self.max_days_per_symbol == 0 {
            return;
        }

        let day_cache = self.entries.entry(ticker).or_default();
        day_cache.shift_remove(&day);
        day_cache.insert(day, ticks);

        while day_cache.len() > self.max_days_per_symbol {
            let _ = day_cache.shift_remove_index(0);
        }
    }
}

impl SyntheticTradeState {
    fn update(&mut self, current_tick: QmtTick, ticker_info: TickerInfo) -> Option<Trade> {
        let trade = self
            .previous_tick
            .as_ref()
            .and_then(|previous_tick| synthesize_trade(previous_tick, &current_tick, ticker_info));
        self.previous_tick = Some(current_tick);
        trade
    }
}

fn qmt_exchange_from_symbol(symbol: &str) -> Option<super::Exchange> {
    if symbol.ends_with(".SH") {
        Some(super::Exchange::SSH)
    } else if symbol.ends_with(".SZ") {
        Some(super::Exchange::SSZ)
    } else {
        None
    }
}

fn is_weekday(day: NaiveDate) -> bool {
    day.weekday().num_days_from_monday() < 5
}

fn trading_day_range_from_timestamps(start_ms: u64, end_ms: u64) -> Option<(NaiveDate, NaiveDate)> {
    if end_ms < start_ms {
        return None;
    }
    Some((
        china_datetime(start_ms)?.date_naive(),
        china_datetime(end_ms)?.date_naive(),
    ))
}

fn merge_trading_day_range(
    ranges: &mut Vec<(NaiveDate, NaiveDate)>,
    start_day: NaiveDate,
    end_day: NaiveDate,
) {
    ranges.push((start_day, end_day));
    ranges.sort_unstable_by_key(|(start, _)| *start);

    let mut merged = Vec::<(NaiveDate, NaiveDate)>::with_capacity(ranges.len());
    for (start, end) in ranges.iter().copied() {
        if let Some((_, previous_end)) = merged.last_mut()
            && start <= previous_end.succ_opt().unwrap_or(*previous_end)
        {
            if end > *previous_end {
                *previous_end = end;
            }
            continue;
        }
        merged.push((start, end));
    }

    *ranges = merged;
}

fn trading_day_range_is_cached(venue: Venue, start_day: NaiveDate, end_day: NaiveDate) -> bool {
    let Ok(cache) = TRADING_DAY_CACHE.read() else {
        return false;
    };
    let Some(entry) = cache.get(&venue) else {
        return false;
    };
    entry
        .covered_ranges
        .iter()
        .any(|(covered_start, covered_end)| *covered_start <= start_day && end_day <= *covered_end)
}

fn cache_trading_days(venue: Venue, start_day: NaiveDate, end_day: NaiveDate, days: &[NaiveDate]) {
    let Ok(mut cache) = TRADING_DAY_CACHE.write() else {
        return;
    };
    let entry = cache.entry(venue).or_default();
    entry.trading_days.extend(days.iter().copied());
    merge_trading_day_range(&mut entry.covered_ranges, start_day, end_day);
}

fn get_cached_tick_day(ticker: Ticker, day: NaiveDate) -> Option<Vec<QmtTick>> {
    let Ok(mut cache) = TICK_DAY_CACHE.write() else {
        return None;
    };
    cache.get(ticker, day)
}

fn cache_tick_day(ticker: Ticker, day: NaiveDate, ticks: Vec<QmtTick>) {
    let Ok(mut cache) = TICK_DAY_CACHE.write() else {
        return;
    };
    cache.insert(ticker, day, ticks);
}

fn merge_ticks(existing: &[QmtTick], incoming: impl IntoIterator<Item = QmtTick>) -> Vec<QmtTick> {
    let mut by_timestamp = FxHashMap::default();

    for tick in existing.iter().cloned() {
        by_timestamp.insert(tick.time, tick);
    }
    for tick in incoming {
        by_timestamp.insert(tick.time, tick);
    }

    let mut merged = by_timestamp.into_values().collect::<Vec<_>>();
    merged.sort_by_key(|tick| tick.time);
    merged
}

fn prune_current_day_tick_cache(cache: &mut FxHashMap<Ticker, CurrentDayTickCacheEntry>) {
    let Some(today) = current_china_day() else {
        cache.clear();
        return;
    };
    cache.retain(|_, entry| entry.day == today);
}

fn cache_live_tick(ticker: Ticker, tick: &QmtTick) {
    let Some(day) = china_trading_day(tick.time) else {
        return;
    };
    if current_china_day() != Some(day) {
        return;
    }

    let Ok(mut cache) = CURRENT_DAY_TICK_CACHE.write() else {
        return;
    };
    prune_current_day_tick_cache(&mut cache);

    let entry = cache
        .entry(ticker)
        .or_insert_with(|| CurrentDayTickCacheEntry {
            day,
            ticks: Vec::new(),
            history_loaded: false,
        });

    if entry.day != day {
        *entry = CurrentDayTickCacheEntry {
            day,
            ticks: Vec::new(),
            history_loaded: false,
        };
    }

    entry.ticks = merge_ticks(&entry.ticks, std::iter::once(tick.clone()));
}

fn merge_current_day_history_and_live(
    ticker: Ticker,
    day: NaiveDate,
    history_ticks: Vec<QmtTick>,
) -> Vec<QmtTick> {
    let Ok(mut cache) = CURRENT_DAY_TICK_CACHE.write() else {
        return history_ticks;
    };
    prune_current_day_tick_cache(&mut cache);

    let entry = cache
        .entry(ticker)
        .or_insert_with(|| CurrentDayTickCacheEntry {
            day,
            ticks: Vec::new(),
            history_loaded: false,
        });

    if entry.day != day {
        *entry = CurrentDayTickCacheEntry {
            day,
            ticks: Vec::new(),
            history_loaded: false,
        };
    }

    entry.ticks = merge_ticks(&history_ticks, entry.ticks.clone());
    entry.history_loaded = true;
    entry.ticks.clone()
}

fn current_day_tick_snapshot(ticker: Ticker, day: NaiveDate) -> Option<Vec<QmtTick>> {
    let Ok(mut cache) = CURRENT_DAY_TICK_CACHE.write() else {
        return None;
    };
    prune_current_day_tick_cache(&mut cache);
    let entry = cache.get(&ticker)?;
    if entry.day != day {
        return None;
    }
    Some(entry.ticks.clone())
}

fn current_day_history_ready(ticker: Ticker, day: NaiveDate) -> bool {
    let Ok(mut cache) = CURRENT_DAY_TICK_CACHE.write() else {
        return false;
    };
    prune_current_day_tick_cache(&mut cache);
    cache.get(&ticker)
        .is_some_and(|entry| entry.day == day && entry.history_loaded)
}

fn build_live_kline_from_ticks(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    latest_tick: &QmtTick,
    ticks: &[QmtTick],
) -> Option<Kline> {
    let venue = ticker_info.exchange().venue();
    let bucket_start = qmt_bucket_start(venue, latest_tick.time, timeframe)?;

    let last_seed_tick = ticks.iter().rev().find(|tick| tick.time < bucket_start).cloned();
    let mut bucket_ticks = ticks
        .iter()
        .filter(|tick| bucket_start <= tick.time && tick.time <= latest_tick.time)
        .cloned()
        .collect::<Vec<_>>();

    if bucket_ticks.is_empty() {
        return None;
    }

    let mut relevant_ticks = Vec::with_capacity(bucket_ticks.len() + usize::from(last_seed_tick.is_some()));
    if let Some(seed_tick) = last_seed_tick.clone() {
        relevant_ticks.push(seed_tick);
    }
    relevant_ticks.append(&mut bucket_ticks);

    let trades = synthesize_trades_from_ticks(&relevant_ticks, ticker_info);
    let mut bars = aggregate_trades_to_klines(
        &trades,
        &relevant_ticks,
        ticker_info,
        timeframe,
        bucket_start,
        latest_tick.time,
    )
    .ok()?;

    if let Some(kline) = bars.pop() {
        return Some(kline);
    }

    let first_bucket_tick = relevant_ticks.iter().find(|tick| tick.time >= bucket_start)?;
    let close = latest_tick.last_price.filter(|price| *price > 0.0)?;
    let open = if let Some(day) = china_trading_day(latest_tick.time) {
        let opening_session_start = qmt_session_bounds(venue, day)
            .and_then(|sessions| sessions.first().copied())
            .map(|(session_start, _)| session_start);
        let opening_bucket = opening_session_start
            .and_then(|session_start| qmt_bucket_start(venue, session_start, timeframe));

        if opening_bucket == Some(bucket_start) {
            first_bucket_tick
                .open
                .or(first_bucket_tick.last_price)
                .filter(|price| *price > 0.0)
        } else {
            last_seed_tick
                .as_ref()
                .and_then(|tick| tick.last_price)
                .filter(|price| *price > 0.0)
                .or(first_bucket_tick.last_price.filter(|price| *price > 0.0))
        }
    } else {
        first_bucket_tick.last_price.filter(|price| *price > 0.0)
    }?;

    let mut high = close;
    let mut low = close;
    for tick in relevant_ticks.iter().filter(|tick| tick.time >= bucket_start) {
        if let Some(last_price) = tick.last_price.filter(|price| *price > 0.0) {
            high = high.max(last_price);
            low = low.min(last_price);
        }
    }

    let baseline_volume = last_seed_tick.and_then(|tick| tick.volume).unwrap_or(0);
    let current_volume = latest_tick
        .volume
        .unwrap_or(baseline_volume)
        .saturating_sub(baseline_volume) as f32
        * QMT_VOLUME_LOT_SIZE;

    Some(Kline::new(
        bucket_start,
        open,
        high,
        low,
        close,
        Volume::TotalOnly(Qty::from_f32(current_volume).round_to_min_qty(ticker_info.min_qty)),
        ticker_info.min_ticksize,
    ))
}

fn build_live_kline_snapshot(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    latest_tick: &QmtTick,
) -> Option<Kline> {
    let day = china_trading_day(latest_tick.time)?;
    let ticks = current_day_tick_snapshot(ticker_info.ticker, day)?;
    build_live_kline_from_ticks(ticker_info, timeframe, latest_tick, &ticks)
}

fn recent_tick_fetch_failure(ticker: Ticker, day: NaiveDate) -> Option<String> {
    let Ok(mut cache) = TICK_FETCH_FAILURE_CACHE.write() else {
        return None;
    };

    let now = Instant::now();
    cache.retain(|_, entry| now.duration_since(entry.failed_at) < QMT_TICK_FETCH_FAILURE_COOLDOWN);
    cache.get(&(ticker, day)).map(|entry| entry.error.clone())
}

fn cache_tick_fetch_failure(ticker: Ticker, day: NaiveDate, error: &AdapterError) {
    let Ok(mut cache) = TICK_FETCH_FAILURE_CACHE.write() else {
        return;
    };
    cache.insert(
        (ticker, day),
        TickFetchFailureEntry {
            failed_at: Instant::now(),
            error: error.to_string(),
        },
    );
}

fn clear_tick_fetch_failure(ticker: Ticker, day: NaiveDate) {
    let Ok(mut cache) = TICK_FETCH_FAILURE_CACHE.write() else {
        return;
    };
    cache.remove(&(ticker, day));
}

fn is_qmt_trading_day(venue: Venue, day: NaiveDate) -> bool {
    let Ok(cache) = TRADING_DAY_CACHE.read() else {
        return is_weekday(day);
    };
    let Some(entry) = cache.get(&venue) else {
        return is_weekday(day);
    };

    if entry
        .covered_ranges
        .iter()
        .any(|(covered_start, covered_end)| *covered_start <= day && day <= *covered_end)
    {
        return entry.trading_days.contains(&day);
    }

    is_weekday(day)
}

fn china_trading_day(timestamp_ms: u64) -> Option<chrono::NaiveDate> {
    let adjusted =
        chrono::DateTime::from_timestamp_millis(timestamp_ms as i64)? + chrono::Duration::hours(8);
    Some(adjusted.date_naive())
}

fn china_offset() -> Option<FixedOffset> {
    FixedOffset::east_opt(8 * 60 * 60)
}

fn current_china_day() -> Option<NaiveDate> {
    Some(chrono::Utc::now().with_timezone(&china_offset()?).date_naive())
}

fn china_datetime(timestamp_ms: u64) -> Option<chrono::DateTime<FixedOffset>> {
    let offset = china_offset()?;
    chrono::DateTime::from_timestamp_millis(timestamp_ms as i64).map(|dt| dt.with_timezone(&offset))
}

fn qmt_timeframe_ms(timeframe: Timeframe) -> Option<u64> {
    match timeframe {
        Timeframe::MS100
        | Timeframe::MS200
        | Timeframe::MS300
        | Timeframe::MS500
        | Timeframe::MS1000
        | Timeframe::MS3000 => None,
        _ => Some(timeframe.to_milliseconds()),
    }
}

fn qmt_default_kline_range(timeframe: Timeframe) -> Option<(u64, u64)> {
    let interval_ms = qmt_timeframe_ms(timeframe)?;
    let end = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let start = end.saturating_sub(DEFAULT_QMT_INITIAL_KLINE_BARS * interval_ms);
    Some((start, end))
}

fn qmt_session_bounds(venue: Venue, day: NaiveDate) -> Option<[(u64, u64); 2]> {
    if !is_qmt_trading_day(venue, day) {
        return None;
    }

    let offset = china_offset()?;
    let morning_start = offset
        .from_local_datetime(&day.and_hms_opt(9, 30, 0)?)
        .single()?
        .timestamp_millis() as u64;
    let morning_end = offset
        .from_local_datetime(&day.and_hms_opt(11, 30, 0)?)
        .single()?
        .timestamp_millis() as u64;
    let afternoon_start = offset
        .from_local_datetime(&day.and_hms_opt(13, 0, 0)?)
        .single()?
        .timestamp_millis() as u64;
    let afternoon_end = offset
        .from_local_datetime(&day.and_hms_opt(15, 0, 0)?)
        .single()?
        .timestamp_millis() as u64;
    Some([
        (morning_start, morning_end),
        (afternoon_start, afternoon_end),
    ])
}

fn qmt_bucket_start(venue: Venue, timestamp_ms: u64, timeframe: Timeframe) -> Option<u64> {
    let dt = china_datetime(timestamp_ms)?;
    if !is_qmt_trading_day(venue, dt.date_naive()) {
        return None;
    }

    if timeframe == Timeframe::D1 {
        let offset = china_offset()?;
        return offset
            .from_local_datetime(&dt.date_naive().and_hms_opt(0, 0, 0)?)
            .single()
            .map(|value| value.timestamp_millis() as u64);
    }

    let interval_ms = qmt_timeframe_ms(timeframe)?;
    let sessions = qmt_session_bounds(venue, dt.date_naive())?;
    let final_session_end = sessions.last().map(|(_, session_end)| *session_end)?;
    let mut effective_ts = timestamp_ms;
    if final_session_end <= timestamp_ms
        && timestamp_ms <= final_session_end.saturating_add(QMT_CLOSE_BUCKET_GRACE_MS)
    {
        effective_ts = final_session_end.saturating_sub(1);
    }

    for (session_start, session_end) in sessions {
        if session_start <= effective_ts && effective_ts < session_end {
            return Some(
                session_start + ((effective_ts - session_start) / interval_ms) * interval_ms,
            );
        }
    }

    None
}

pub fn is_trading_bucket_start(venue: Venue, timestamp_ms: u64, timeframe: Timeframe) -> bool {
    qmt_bucket_start(venue, timestamp_ms, timeframe) == Some(timestamp_ms)
}

pub fn uses_gapless_time_axis(venue: Venue) -> bool {
    matches!(venue, Venue::SSH | Venue::SSZ)
}

pub fn supports_gapless_time_axis_timeframe(venue: Venue, timeframe: Timeframe) -> bool {
    uses_gapless_time_axis(venue) && qmt_timeframe_ms(timeframe).is_some()
}

fn qmt_bucket_starts_for_day(venue: Venue, day: NaiveDate, timeframe: Timeframe) -> Option<Vec<u64>> {
    if !is_qmt_trading_day(venue, day) {
        return None;
    }

    if timeframe == Timeframe::D1 {
        let offset = china_offset()?;
        return offset
            .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
            .single()
            .map(|value| vec![value.timestamp_millis() as u64]);
    }

    let interval_ms = qmt_timeframe_ms(timeframe)?;
    let sessions = qmt_session_bounds(venue, day)?;
    let mut starts = Vec::new();

    for (session_start, session_end) in sessions {
        let mut bucket = session_start;
        while bucket < session_end {
            starts.push(bucket);
            bucket = bucket.saturating_add(interval_ms);
        }
    }

    Some(starts)
}

fn qmt_bucket_position(
    venue: Venue,
    timestamp_ms: u64,
    timeframe: Timeframe,
) -> Option<(NaiveDate, usize)> {
    let day = china_trading_day(timestamp_ms)?;
    let bucket = qmt_bucket_start(venue, timestamp_ms, timeframe)?;
    let starts = qmt_bucket_starts_for_day(venue, day, timeframe)?;
    starts
        .iter()
        .position(|candidate| *candidate == bucket)
        .map(|index| (day, index))
}

fn qmt_trading_day_distance(venue: Venue, from_day: NaiveDate, to_day: NaiveDate) -> i64 {
    if from_day == to_day {
        return 0;
    }

    if from_day < to_day {
        let mut count = 0_i64;
        let mut day = from_day;
        while day < to_day {
            let Some(next_day) = day.checked_add_days(Days::new(1)) else {
                break;
            };
            day = next_day;
            if is_qmt_trading_day(venue, day) {
                count += 1;
            }
        }
        return count;
    }

    -qmt_trading_day_distance(venue, to_day, from_day)
}

fn qmt_shift_trading_day(venue: Venue, day: NaiveDate, offset: i64) -> Option<NaiveDate> {
    if offset == 0 {
        return is_qmt_trading_day(venue, day).then_some(day);
    }

    let mut current = day;
    let mut remaining = offset.unsigned_abs();
    let forward = offset > 0;

    while remaining > 0 {
        current = if forward {
            current.checked_add_days(Days::new(1))?
        } else {
            current.checked_sub_days(Days::new(1))?
        };

        if is_qmt_trading_day(venue, current) {
            remaining -= 1;
        }
    }

    Some(current)
}

pub fn time_axis_bucket_offset(
    venue: Venue,
    anchor_ms: u64,
    target_ms: u64,
    timeframe: Timeframe,
) -> Option<i64> {
    if !uses_gapless_time_axis(venue) {
        return None;
    }

    let (anchor_day, anchor_bucket_index) = qmt_bucket_position(venue, anchor_ms, timeframe)?;
    let (target_day, target_bucket_index) = qmt_bucket_position(venue, target_ms, timeframe)?;
    let buckets_per_day = i64::try_from(qmt_bucket_starts_for_day(venue, anchor_day, timeframe)?.len())
        .ok()?;
    if buckets_per_day <= 0 {
        return None;
    }

    let day_offset = qmt_trading_day_distance(venue, anchor_day, target_day);
    Some(
        day_offset * buckets_per_day + i64::try_from(target_bucket_index).ok()?
            - i64::try_from(anchor_bucket_index).ok()?,
    )
}

pub fn time_axis_bucket_at_offset(
    venue: Venue,
    anchor_ms: u64,
    timeframe: Timeframe,
    bucket_offset: i64,
) -> Option<u64> {
    if !uses_gapless_time_axis(venue) {
        return None;
    }

    let (anchor_day, anchor_bucket_index) = qmt_bucket_position(venue, anchor_ms, timeframe)?;
    let buckets_per_day = i64::try_from(qmt_bucket_starts_for_day(venue, anchor_day, timeframe)?.len())
        .ok()?;
    if buckets_per_day <= 0 {
        return None;
    }

    let total_offset = i64::try_from(anchor_bucket_index).ok()? + bucket_offset;
    let day_offset = total_offset.div_euclid(buckets_per_day);
    let bucket_index = usize::try_from(total_offset.rem_euclid(buckets_per_day)).ok()?;
    let day = qmt_shift_trading_day(venue, anchor_day, day_offset)?;
    qmt_bucket_starts_for_day(venue, day, timeframe)?
        .get(bucket_index)
        .copied()
}

fn qmt_trading_bucket_starts(
    venue: Venue,
    start_ms: u64,
    end_ms: u64,
    timeframe: Timeframe,
) -> Vec<u64> {
    if end_ms < start_ms {
        return Vec::new();
    }

    let Some(start_dt) = china_datetime(start_ms) else {
        return Vec::new();
    };
    let Some(end_dt) = china_datetime(end_ms) else {
        return Vec::new();
    };
    let Some(interval_ms) = qmt_timeframe_ms(timeframe) else {
        return Vec::new();
    };

    let mut day = start_dt.date_naive();
    let last_day = end_dt.date_naive();
    let mut buckets = Vec::new();

    while day <= last_day {
        if timeframe == Timeframe::D1 {
            if let Some(bucket) = qmt_bucket_start(
                venue,
                china_offset()
                    .and_then(|offset| {
                        offset
                            .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
                            .single()
                    })
                    .map(|value| value.timestamp_millis() as u64)
                    .unwrap_or(0),
                timeframe,
            ) && bucket <= end_ms
                && bucket.saturating_add(interval_ms) > start_ms
            {
                buckets.push(bucket);
            }
        } else if let Some(sessions) = qmt_session_bounds(venue, day) {
            for (session_start, session_end) in sessions {
                let mut bucket = if start_ms > session_start {
                    session_start + ((start_ms - session_start) / interval_ms) * interval_ms
                } else {
                    session_start
                };

                if bucket >= session_end {
                    continue;
                }

                while bucket < session_end && bucket <= end_ms {
                    if bucket.saturating_add(interval_ms) > start_ms {
                        buckets.push(bucket);
                    }
                    bucket += interval_ms;
                }
            }
        }

        let Some(next_day) = day.checked_add_days(Days::new(1)) else {
            break;
        };
        day = next_day;
    }

    buckets
}

fn qmt_trading_days_between(
    venue: Venue,
    start_day: NaiveDate,
    end_day: NaiveDate,
) -> Vec<NaiveDate> {
    let mut day = start_day;
    let mut trading_days = Vec::new();

    while day <= end_day {
        if is_qmt_trading_day(venue, day) {
            trading_days.push(day);
        }

        let Some(next_day) = day.checked_add_days(Days::new(1)) else {
            break;
        };
        day = next_day;
    }

    trading_days
}

fn qmt_tick_fetch_bounds(venue: Venue, day: NaiveDate) -> Option<(u64, u64)> {
    let sessions = qmt_session_bounds(venue, day)?;
    Some((sessions[0].0, sessions[1].1))
}

fn qmt_current_day_history_bounds(day: NaiveDate) -> Option<(u64, u64)> {
    let offset = china_offset()?;
    let start = offset
        .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
        .single()?
        .timestamp_millis() as u64;
    let end = offset
        .from_local_datetime(&day.and_hms_opt(16, 0, 0)?)
        .single()?
        .timestamp_millis() as u64;
    Some((start, end))
}

fn qmt_latest_history_chunk_range(
    venue: Venue,
    requested_start: u64,
    requested_end: u64,
) -> Option<(u64, u64)> {
    let (start_day, end_day) = trading_day_range_from_timestamps(requested_start, requested_end)?;
    let trading_days = qmt_trading_days_between(venue, start_day, end_day);
    for day in trading_days.into_iter().rev() {
        let (day_start, day_end) = qmt_tick_fetch_bounds(venue, day)?;
        let chunk_start = requested_start.max(day_start);
        let chunk_end = requested_end.min(day_end);

        // Treat the requested end as an exclusive upper bound for backfill.
        // This lets a request ending exactly at an already-loaded bucket start
        // select the previous trading day instead of accidentally falling back
        // to the entire multi-day range.
        if chunk_end > chunk_start {
            return Some((chunk_start, chunk_end));
        }
    }

    None
}

fn qmt_kline_seed_start(venue: Venue, start_ms: u64) -> Option<u64> {
    let start_day = china_trading_day(start_ms)?;

    if current_china_day() == Some(start_day)
        && let Some((current_day_start, _)) = qmt_tick_fetch_bounds(venue, start_day)
        && start_ms <= current_day_start
    {
        return qmt_current_day_history_bounds(start_day).map(|(start, _)| start);
    }

    if is_qmt_trading_day(venue, start_day)
        && let Some((current_day_start, _)) = qmt_tick_fetch_bounds(venue, start_day)
    {
        if current_day_start < start_ms {
            return Some(current_day_start);
        }

        if let Some(previous_day) = qmt_shift_trading_day(venue, start_day, -1)
            && let Some((previous_day_start, _)) = qmt_tick_fetch_bounds(venue, previous_day)
        {
            return Some(previous_day_start);
        }

        return Some(current_day_start);
    }

    qmt_shift_trading_day(venue, start_day, -1)
        .and_then(|previous_day| qmt_tick_fetch_bounds(venue, previous_day))
        .map(|(previous_day_start, _)| previous_day_start)
}

async fn ensure_trading_calendar(
    venue: Venue,
    start_ms: u64,
    end_ms: u64,
) -> Result<(), AdapterError> {
    let Some((start_day, end_day)) = trading_day_range_from_timestamps(start_ms, end_ms) else {
        return Ok(());
    };

    if trading_day_range_is_cached(venue, start_day, end_day) {
        return Ok(());
    }

    let days = fetch_trading_days(venue, start_ms, end_ms).await?;
    cache_trading_days(venue, start_day, end_day, &days);
    Ok(())
}

async fn fetch_trading_days(
    venue: Venue,
    start_ms: u64,
    end_ms: u64,
) -> Result<Vec<NaiveDate>, AdapterError> {
    let url = qmt_bridge_http_url(
        "/api/v1/trading_days",
        &[
            ("venue", venue.to_string()),
            ("start", start_ms.to_string()),
            ("end", end_ms.to_string()),
        ],
    )?;

    let response = reqwest::get(&url).await.map_err(AdapterError::from)?;
    let status = response.status();
    let text = response.text().await.map_err(AdapterError::from)?;

    if !status.is_success() {
        return Err(AdapterError::http_status_failed(
            status,
            format!("GET {url} failed: {text}"),
        ));
    }

    let parsed: BridgeItemsResponse<String> =
        serde_json::from_str(&text).map_err(|e| AdapterError::ParseError(e.to_string()))?;

    parsed
        .items
        .into_iter()
        .map(|day| {
            NaiveDate::parse_from_str(&day, "%Y%m%d")
                .map_err(|e| AdapterError::ParseError(format!("invalid trading day {day}: {e}")))
        })
        .collect()
}

fn build_daily_seed_prices(ticks: &[QmtTick]) -> HashMap<NaiveDate, f32> {
    let mut seeds = HashMap::new();

    for tick in ticks {
        let Some(day) = china_trading_day(tick.time) else {
            continue;
        };
        if seeds.contains_key(&day) {
            continue;
        }

        if let Some(price) = tick
            .last_close
            .or(tick.last_price)
            .filter(|price| *price > 0.0)
        {
            seeds.insert(day, price);
        }
    }

    seeds
}

fn synthesize_trades_from_ticks(ticks: &[QmtTick], ticker_info: TickerInfo) -> Vec<Trade> {
    ticks
        .windows(2)
        .filter_map(|pair| {
            let [previous_tick, current_tick] = pair else {
                return None;
            };
            synthesize_trade(previous_tick, current_tick, ticker_info)
        })
        .collect()
}

fn aggregate_trades_to_klines(
    trades: &[Trade],
    ticks: &[QmtTick],
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    start_ms: u64,
    end_ms: u64,
) -> Result<Vec<Kline>, AdapterError> {
    if end_ms < start_ms {
        return Ok(Vec::new());
    }

    let interval_ms = qmt_timeframe_ms(timeframe).ok_or_else(|| {
        AdapterError::InvalidRequest(format!(
            "unsupported QMT timeframe for synthetic klines: {timeframe}"
        ))
    })?;

    #[derive(Clone, Copy)]
    struct AggBar {
        time: u64,
        open: f32,
        high: f32,
        low: f32,
        close: f32,
        volume: f32,
    }

    let mut bars = HashMap::<u64, AggBar>::new();
    let venue = ticker_info.exchange().venue();
    for trade in trades.iter().copied() {
        let Some(bucket) = qmt_bucket_start(venue, trade.time, timeframe) else {
            continue;
        };
        if bucket > end_ms || bucket.saturating_add(interval_ms) <= start_ms {
            continue;
        }

        let trade_price = trade.price.to_f32();
        let trade_qty: f32 = trade.qty.into();
        if trade_qty <= 0.0 || trade_price <= 0.0 {
            continue;
        }

        match bars.get_mut(&bucket) {
            Some(bar) => {
                bar.high = bar.high.max(trade_price);
                bar.low = bar.low.min(trade_price);
                bar.close = trade_price;
                bar.volume += trade_qty;
            }
            None => {
                bars.insert(
                    bucket,
                    AggBar {
                        time: bucket,
                        open: trade_price,
                        high: trade_price,
                        low: trade_price,
                        close: trade_price,
                        volume: trade_qty,
                    },
                );
            }
        }
    }

    for tick in ticks {
        let Some(bucket) = qmt_bucket_start(venue, tick.time, timeframe) else {
            continue;
        };
        if bucket > end_ms || bucket.saturating_add(interval_ms) <= start_ms {
            continue;
        }

        let Some(bar) = bars.get_mut(&bucket) else {
            continue;
        };

        if let Some(last_price) = tick.last_price.filter(|price| *price > 0.0) {
            bar.high = bar.high.max(last_price);
            bar.low = bar.low.min(last_price);
        }
    }

    for pair in ticks.windows(2) {
        let [previous_tick, current_tick] = pair else {
            continue;
        };
        let Some(bucket) = qmt_bucket_start(venue, current_tick.time, timeframe) else {
            continue;
        };
        if bucket > end_ms || bucket.saturating_add(interval_ms) <= start_ms {
            continue;
        }

        let Some(bar) = bars.get_mut(&bucket) else {
            continue;
        };
        let day_changed = china_trading_day(previous_tick.time) != china_trading_day(current_tick.time);

        if let Some(current_high) = current_tick.high.filter(|price| *price > 0.0) {
            let high_increased = day_changed
                || matches!(
                    previous_tick.high,
                    Some(previous_high) if current_high > previous_high + f32::EPSILON
                )
                || previous_tick.high.is_none();
            if high_increased {
                bar.high = bar.high.max(current_high);
            }
        }

        if let Some(current_low) = current_tick.low.filter(|price| *price > 0.0) {
            let low_decreased = day_changed
                || matches!(
                    previous_tick.low,
                    Some(previous_low) if current_low + f32::EPSILON < previous_low
                )
                || previous_tick.low.is_none();
            if low_decreased {
                bar.low = bar.low.min(current_low);
            }
        }
    }

    for tick in ticks {
        let Some(bucket) = qmt_bucket_start(venue, tick.time, timeframe) else {
            continue;
        };
        if bucket > end_ms || bucket.saturating_add(interval_ms) <= start_ms {
            continue;
        }

        let Some(bar) = bars.get_mut(&bucket) else {
            continue;
        };

        let Some(day) = china_trading_day(tick.time) else {
            continue;
        };
        let Some(sessions) = qmt_session_bounds(venue, day) else {
            continue;
        };
        let Some(opening_bucket) = qmt_bucket_start(venue, sessions[0].0, timeframe) else {
            continue;
        };
        if bucket != opening_bucket {
            continue;
        }

        if let Some(open_price) = tick.open.filter(|price| *price > 0.0) {
            bar.open = open_price;
            bar.high = bar.high.max(open_price);
            bar.low = bar.low.min(open_price);
        }
    }

    let seed_prices = build_daily_seed_prices(ticks);
    let bucket_starts = qmt_trading_bucket_starts(venue, start_ms, end_ms, timeframe);
    if bucket_starts.is_empty() {
        return Ok(Vec::new());
    }

    let mut previous_close = None::<f32>;
    let mut klines = Vec::with_capacity(bucket_starts.len());
    for bucket in bucket_starts {
        if let Some(bar) = bars.remove(&bucket) {
            previous_close = Some(bar.close);
            klines.push(Kline::new(
                bar.time,
                bar.open,
                bar.high,
                bar.low,
                bar.close,
                Volume::TotalOnly(Qty::from_f32(bar.volume).round_to_min_qty(ticker_info.min_qty)),
                ticker_info.min_ticksize,
            ));
            continue;
        }

        let Some(day) = china_trading_day(bucket) else {
            continue;
        };
        if previous_close.is_none() {
            previous_close = seed_prices.get(&day).copied();
        }
        let Some(close) = previous_close else {
            continue;
        };

        klines.push(Kline::new(
            bucket,
            close,
            close,
            close,
            close,
            Volume::TotalOnly(Qty::ZERO),
            ticker_info.min_ticksize,
        ));
    }

    Ok(klines)
}

fn synthesize_trade(
    previous_tick: &QmtTick,
    current_tick: &QmtTick,
    ticker_info: TickerInfo,
) -> Option<Trade> {
    let day_changed = china_trading_day(previous_tick.time) != china_trading_day(current_tick.time);

    let volume_reset = matches!(
        (previous_tick.volume, current_tick.volume),
        (Some(prev), Some(curr)) if curr < prev
    );
    let amount_reset = matches!(
        (previous_tick.amount, current_tick.amount),
        (Some(prev), Some(curr)) if curr + f32::EPSILON < prev
    );
    let txn_reset = matches!(
        (previous_tick.transaction_num, current_tick.transaction_num),
        (Some(prev), Some(curr)) if curr < prev
    );
    let counters_reset = volume_reset || amount_reset || txn_reset;

    if counters_reset && !day_changed {
        return None;
    }

    let baseline_volume = if counters_reset && day_changed {
        Some(0)
    } else {
        previous_tick.volume
    };
    let qty_raw = match (current_tick.volume, baseline_volume) {
        (Some(curr), Some(base)) if curr > base => Some((curr - base) as f32 * QMT_VOLUME_LOT_SIZE),
        (Some(curr), None) if curr > 0 => Some(curr as f32 * QMT_VOLUME_LOT_SIZE),
        _ => None,
    }?;

    if qty_raw <= 0.0 {
        return None;
    }

    let raw_price = current_tick.last_price?;

    if raw_price <= 0.0 {
        return None;
    }

    let is_sell = infer_is_sell(previous_tick, current_tick, raw_price);
    let price = Price::from_f32(raw_price).round_to_min_tick(ticker_info.min_ticksize);
    let qty = Qty::from_f32(qty_raw).round_to_min_qty(ticker_info.min_qty);

    if qty.is_zero() {
        return None;
    }

    Some(Trade {
        time: current_tick.time,
        is_sell,
        price,
        qty,
    })
}

fn infer_is_sell(previous_tick: &QmtTick, current_tick: &QmtTick, synthetic_price: f32) -> bool {
    let bid1 = current_tick.bid_price.first().copied();
    let ask1 = current_tick.ask_price.first().copied();

    if let Some(ask) = ask1
        && synthetic_price >= ask
    {
        return false;
    }
    if let Some(bid) = bid1
        && synthetic_price <= bid
    {
        return true;
    }

    if let (Some(bid), Some(ask)) = (bid1, ask1) {
        let mid = (bid + ask) / 2.0;
        if synthetic_price > mid {
            return false;
        }
        if synthetic_price < mid {
            return true;
        }
    }

    if let (Some(previous_last), Some(current_last)) =
        (previous_tick.last_price, current_tick.last_price)
    {
        if current_last > previous_last {
            return false;
        }
        if current_last < previous_last {
            return true;
        }
    }

    let bid_drop = match (previous_tick.bid_vol.first(), current_tick.bid_vol.first()) {
        (Some(prev), Some(curr)) => prev - curr,
        _ => 0.0,
    };
    let ask_drop = match (previous_tick.ask_vol.first(), current_tick.ask_vol.first()) {
        (Some(prev), Some(curr)) => prev - curr,
        _ => 0.0,
    };

    if bid_drop > ask_drop {
        true
    } else if ask_drop > bid_drop {
        false
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn sample_ticker_info() -> TickerInfo {
        TickerInfo::new(Ticker::new("600309.SH", super::super::Exchange::SSH), 0.01, 1.0, None)
    }

    fn china_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> u64 {
        let day = NaiveDate::from_ymd_opt(year, month, day).expect("valid date");
        china_offset()
            .and_then(|offset| {
                offset
                    .from_local_datetime(
                        &day.and_hms_opt(hour, minute, second)
                            .expect("valid local datetime"),
                    )
                    .single()
            })
            .map(|dt| dt.timestamp_millis() as u64)
            .expect("valid china timestamp")
    }

    fn sample_tick(time: u64) -> QmtTick {
        QmtTick {
            time,
            last_price: Some(82.0),
            open: None,
            high: Some(82.0),
            low: Some(82.0),
            last_close: Some(82.38),
            amount: Some(0.0),
            volume: Some(0),
            transaction_num: Some(0),
            ask_price: Vec::new(),
            bid_price: Vec::new(),
            ask_vol: Vec::new(),
            bid_vol: Vec::new(),
        }
    }

    #[test]
    fn synthesize_trade_uses_last_price_with_volume_qty() {
        let ticker_info = sample_ticker_info();
        let mut previous = sample_tick(1_775_525_401_000);
        previous.amount = Some(24_196_695.0);
        previous.volume = Some(2_958);
        previous.transaction_num = Some(258);

        let mut current = sample_tick(1_775_525_404_000);
        current.last_price = Some(82.08);
        current.amount = Some(25_976_594.0);
        current.volume = Some(3_175);
        current.transaction_num = Some(284);

        let trade = synthesize_trade(&previous, &current, ticker_info)
            .expect("expected synthetic trade from volume delta");

        assert_eq!(trade.time, current.time);
        assert_eq!(f32::from(trade.qty), 21_700.0);
        assert_eq!(trade.price.to_f32(), 82.08);
    }

    #[test]
    fn synthesize_trade_requires_current_last_price() {
        let ticker_info = sample_ticker_info();
        let mut previous = sample_tick(1_775_525_401_000);
        previous.last_price = Some(82.00);
        previous.volume = Some(2_958);
        previous.transaction_num = Some(258);

        let mut current = sample_tick(1_775_525_404_000);
        current.last_price = None;
        current.volume = Some(3_175);
        current.transaction_num = Some(284);

        assert!(synthesize_trade(&previous, &current, ticker_info).is_none());
    }

    #[test]
    fn aggregate_trades_to_klines_uses_tick_extrema() {
        let ticker_info = sample_ticker_info();
        let start = 1_775_525_400_000_u64;
        let end = start + 300_000;

        let trades = vec![
            Trade {
                time: start + 1_000,
                is_sell: false,
                price: Price::from_f32(82.00),
                qty: Qty::from_f32(100.0),
            },
            Trade {
                time: start + 4_000,
                is_sell: true,
                price: Price::from_f32(82.02),
                qty: Qty::from_f32(100.0),
            },
        ];

        let mut tick1 = sample_tick(start + 1_000);
        tick1.last_price = Some(82.00);
        tick1.high = Some(82.00);
        tick1.low = Some(82.00);

        let mut tick2 = sample_tick(start + 13_000);
        tick2.last_price = Some(82.09);
        tick2.high = Some(82.37);
        tick2.low = Some(81.80);

        let bars =
            aggregate_trades_to_klines(&trades, &[tick1, tick2], ticker_info, Timeframe::M5, start, end)
                .expect("expected synthetic klines");

        let first = bars.first().expect("expected one bar");
        assert_eq!(first.open.to_f32(), 82.00);
        assert_eq!(first.close.to_f32(), 82.02);
        assert_eq!(first.high.to_f32(), 82.37);
        assert!((first.low.to_f32() - 81.80).abs() < 0.001);
    }

    #[test]
    fn aggregate_trades_to_klines_uses_tick_open_for_opening_bucket() {
        let ticker_info = sample_ticker_info();
        let start = china_ms(2026, 4, 9, 9, 30, 0);
        let end = start + 300_000;

        let trades = vec![
            Trade {
                time: start + 1_000,
                is_sell: false,
                price: Price::from_f32(82.00),
                qty: Qty::from_f32(100.0),
            },
            Trade {
                time: start + 4_000,
                is_sell: true,
                price: Price::from_f32(82.02),
                qty: Qty::from_f32(100.0),
            },
        ];

        let mut tick1 = sample_tick(start + 1_000);
        tick1.open = Some(81.80);
        tick1.last_price = Some(82.00);
        tick1.high = Some(82.00);
        tick1.low = Some(81.80);

        let mut tick2 = sample_tick(start + 13_000);
        tick2.open = Some(81.80);
        tick2.last_price = Some(82.09);
        tick2.high = Some(82.37);
        tick2.low = Some(81.80);

        let bars =
            aggregate_trades_to_klines(&trades, &[tick1, tick2], ticker_info, Timeframe::M5, start, end)
                .expect("expected synthetic klines");

        let first = bars.first().expect("expected one bar");
        assert!((first.open.to_f32() - 81.80).abs() < 0.001);
        assert_eq!(first.high.to_f32(), 82.37);
        assert!((first.low.to_f32() - 81.80).abs() < 0.001);
        assert_eq!(first.close.to_f32(), 82.02);
    }

    #[test]
    fn qmt_timeframe_ms_supports_m3() {
        assert_eq!(qmt_timeframe_ms(Timeframe::M3), Some(180_000));
    }

    #[test]
    fn qmt_timeframe_ms_supports_custom_minutes() {
        assert_eq!(
            qmt_timeframe_ms(Timeframe::CustomMinutes(45)),
            Some(2_700_000)
        );
    }

    #[test]
    fn supports_gapless_time_axis_timeframe_excludes_heatmap_ms3000() {
        assert!(!supports_gapless_time_axis_timeframe(
            Venue::SSH,
            Timeframe::MS3000
        ));
        assert!(supports_gapless_time_axis_timeframe(
            Venue::SSH,
            Timeframe::M3
        ));
    }

    #[test]
    fn qmt_kline_seed_start_uses_same_day_session_start_mid_session() {
        let start = china_ms(2026, 4, 9, 10, 15, 0);
        let expected = china_ms(2026, 4, 9, 9, 30, 0);
        assert_eq!(qmt_kline_seed_start(Venue::SSH, start), Some(expected));
    }

    #[test]
    fn qmt_kline_seed_start_uses_previous_trading_day_at_open() {
        let start = china_ms(2026, 4, 9, 9, 30, 0);
        let expected = china_ms(2026, 4, 8, 9, 30, 0);
        assert_eq!(qmt_kline_seed_start(Venue::SSH, start), Some(expected));
    }

    #[test]
    fn qmt_bucket_start_maps_close_grace_to_last_bucket() {
        let closing_tick = china_ms(2026, 4, 9, 15, 0, 2);
        let expected = china_ms(2026, 4, 9, 14, 30, 0);
        assert_eq!(
            qmt_bucket_start(Venue::SSH, closing_tick, Timeframe::M30),
            Some(expected)
        );
    }

    #[test]
    fn qmt_bucket_start_does_not_map_lunch_grace_to_morning_bucket() {
        let lunch_tick = china_ms(2026, 4, 9, 11, 30, 2);
        assert_eq!(qmt_bucket_start(Venue::SSH, lunch_tick, Timeframe::M30), None);
    }

    #[test]
    fn qmt_bucket_start_maps_custom_close_grace_to_last_bucket() {
        let closing_tick = china_ms(2026, 4, 9, 15, 0, 2);
        let expected = china_ms(2026, 4, 9, 14, 30, 0);
        assert_eq!(
            qmt_bucket_start(Venue::SSH, closing_tick, Timeframe::CustomMinutes(45)),
            Some(expected)
        );
    }

    #[test]
    fn qmt_latest_history_chunk_range_selects_latest_trading_day() {
        if let Ok(mut cache) = TRADING_DAY_CACHE.write() {
            cache.clear();
        }

        let start_day = NaiveDate::from_ymd_opt(2026, 3, 30).expect("valid date");
        let end_day = NaiveDate::from_ymd_opt(2026, 4, 3).expect("valid date");
        let trading_days = [
            NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
            NaiveDate::from_ymd_opt(2026, 3, 31).unwrap(),
            NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 4, 2).unwrap(),
            NaiveDate::from_ymd_opt(2026, 4, 3).unwrap(),
        ];
        cache_trading_days(Venue::SSH, start_day, end_day, &trading_days);

        let requested_start = china_ms(2026, 3, 30, 9, 30, 0);
        let requested_end = china_ms(2026, 4, 3, 15, 0, 0);
        let chunk = qmt_latest_history_chunk_range(Venue::SSH, requested_start, requested_end)
            .expect("expected latest trading day chunk");

        assert_eq!(chunk.0, china_ms(2026, 4, 3, 9, 30, 0));
        assert_eq!(chunk.1, china_ms(2026, 4, 3, 15, 0, 0));
    }

    #[test]
    fn qmt_latest_history_chunk_range_skips_empty_latest_day_overlap() {
        if let Ok(mut cache) = TRADING_DAY_CACHE.write() {
            cache.clear();
        }

        let start_day = NaiveDate::from_ymd_opt(2026, 3, 30).expect("valid date");
        let end_day = NaiveDate::from_ymd_opt(2026, 4, 7).expect("valid date");
        let trading_days = [
            NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
            NaiveDate::from_ymd_opt(2026, 3, 31).unwrap(),
            NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 4, 2).unwrap(),
            NaiveDate::from_ymd_opt(2026, 4, 3).unwrap(),
            NaiveDate::from_ymd_opt(2026, 4, 7).unwrap(),
        ];
        cache_trading_days(Venue::SSH, start_day, end_day, &trading_days);

        let requested_start = china_ms(2026, 3, 30, 9, 30, 0);
        let requested_end = china_ms(2026, 4, 7, 9, 30, 0);
        let chunk = qmt_latest_history_chunk_range(Venue::SSH, requested_start, requested_end)
            .expect("expected latest non-empty trading day chunk");

        assert_eq!(chunk.0, china_ms(2026, 4, 3, 9, 30, 0));
        assert_eq!(chunk.1, china_ms(2026, 4, 3, 15, 0, 0));
    }

    #[test]
    fn merge_ticks_keeps_order_and_deduplicates() {
        let mut tick1 = sample_tick(china_ms(2026, 4, 9, 9, 30, 1));
        tick1.volume = Some(100);
        tick1.transaction_num = Some(1);
        tick1.last_price = Some(82.01);

        let mut tick2 = sample_tick(china_ms(2026, 4, 9, 9, 30, 2));
        tick2.volume = Some(120);
        tick2.transaction_num = Some(2);
        tick2.last_price = Some(82.02);

        let mut tick3 = sample_tick(china_ms(2026, 4, 9, 9, 30, 3));
        tick3.volume = Some(140);
        tick3.transaction_num = Some(3);
        tick3.last_price = Some(82.03);

        let merged = merge_ticks(&[tick2.clone()], vec![tick1.clone(), tick2, tick3.clone()]);

        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].time, tick1.time);
        assert_eq!(merged[1].time, china_ms(2026, 4, 9, 9, 30, 2));
        assert_eq!(merged[2].time, tick3.time);
    }

    #[test]
    fn merge_ticks_overrides_same_timestamp() {
        let ts = china_ms(2026, 4, 9, 9, 30, 2);

        let mut history_tick = sample_tick(ts);
        history_tick.volume = Some(120);
        history_tick.transaction_num = Some(2);
        history_tick.last_price = Some(82.02);

        let mut live_tick = sample_tick(ts);
        live_tick.volume = Some(130);
        live_tick.transaction_num = Some(3);
        live_tick.last_price = Some(82.03);

        let merged = merge_ticks(&[history_tick], vec![live_tick.clone()]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].time, ts);
        assert_eq!(merged[0].volume, live_tick.volume);
        assert_eq!(merged[0].transaction_num, live_tick.transaction_num);
        assert_eq!(merged[0].last_price, live_tick.last_price);
    }

    #[test]
    fn current_day_history_ready_only_after_merge() {
        let ticker = Ticker::new("600309.SH", super::super::Exchange::SSH);
        let day = current_china_day().expect("current china day");
        let timestamp = china_offset()
            .expect("china offset")
            .from_local_datetime(&day.and_hms_opt(10, 0, 0).expect("valid time"))
            .single()
            .expect("valid datetime")
            .timestamp_millis() as u64;

        if let Ok(mut cache) = CURRENT_DAY_TICK_CACHE.write() {
            cache.clear();
        }

        let live_tick = sample_tick(timestamp);
        cache_live_tick(ticker, &live_tick);
        assert!(!current_day_history_ready(ticker, day));

        let _ = merge_current_day_history_and_live(ticker, day, vec![live_tick]);
        assert!(current_day_history_ready(ticker, day));
    }

    #[test]
    fn build_live_kline_from_ticks_reconstructs_current_bucket() {
        let ticker_info = sample_ticker_info();
        let bucket_start = china_ms(2026, 4, 9, 10, 0, 0);

        let mut seed_tick = sample_tick(bucket_start - 1_000);
        seed_tick.last_price = Some(82.10);
        seed_tick.volume = Some(1_000);
        seed_tick.transaction_num = Some(10);

        let mut tick1 = sample_tick(bucket_start + 1_000);
        tick1.open = Some(82.00);
        tick1.last_price = Some(82.00);
        tick1.high = Some(82.00);
        tick1.low = Some(82.00);
        tick1.volume = Some(1_010);
        tick1.transaction_num = Some(11);

        let mut tick2 = sample_tick(bucket_start + 4_000);
        tick2.open = Some(82.00);
        tick2.last_price = Some(82.08);
        tick2.high = Some(82.08);
        tick2.low = Some(82.00);
        tick2.volume = Some(1_030);
        tick2.transaction_num = Some(13);

        let kline = build_live_kline_from_ticks(
            ticker_info,
            Timeframe::M30,
            &tick2,
            &[seed_tick, tick1, tick2.clone()],
        )
        .expect("expected live kline snapshot");

        assert_eq!(kline.time, bucket_start);
        assert_eq!(kline.open.to_f32(), 82.00);
        assert_eq!(kline.close.to_f32(), 82.08);
        assert_eq!(kline.high.to_f32(), 82.08);
        assert_eq!(kline.low.to_f32(), 82.00);
        assert!((f32::from(kline.volume.total()) - 3_000.0).abs() < 0.01);
    }

    #[test]
    fn build_live_kline_from_ticks_rolls_to_next_bucket() {
        let ticker_info = sample_ticker_info();
        let first_bucket = china_ms(2026, 4, 9, 9, 30, 0);
        let second_bucket = china_ms(2026, 4, 9, 10, 0, 0);

        let mut tick1 = sample_tick(first_bucket + 1_000);
        tick1.open = Some(82.00);
        tick1.last_price = Some(82.02);
        tick1.volume = Some(1_000);
        tick1.transaction_num = Some(10);

        let mut tick2 = sample_tick(second_bucket + 2_000);
        tick2.open = Some(82.05);
        tick2.last_price = Some(82.06);
        tick2.high = Some(82.06);
        tick2.low = Some(82.05);
        tick2.volume = Some(1_020);
        tick2.transaction_num = Some(11);

        let kline = build_live_kline_from_ticks(
            ticker_info,
            Timeframe::M30,
            &tick2,
            &[tick1, tick2.clone()],
        )
        .expect("expected next bucket kline");

        assert_eq!(kline.time, second_bucket);
        assert_eq!(kline.close.to_f32(), 82.06);
    }
}

fn tick_to_depth_payload(tick: &QmtTick) -> DepthPayload {
    let bids = tick
        .bid_price
        .iter()
        .copied()
        .zip(tick.bid_vol.iter().copied())
        .filter(|(price, qty)| *price > 0.0 && *qty > 0.0)
        .map(|(price, qty)| DeOrder { price, qty })
        .collect::<Vec<_>>();

    let asks = tick
        .ask_price
        .iter()
        .copied()
        .zip(tick.ask_vol.iter().copied())
        .filter(|(price, qty)| *price > 0.0 && *qty > 0.0)
        .map(|(price, qty)| DeOrder { price, qty })
        .collect::<Vec<_>>();

    DepthPayload {
        last_update_id: tick.time,
        time: tick.time,
        bids,
        asks,
    }
}

fn trade_flush_map(ticker_info: TickerInfo) -> FxHashMap<Ticker, (TickerInfo, ())> {
    FxHashMap::from_iter([(ticker_info.ticker, (ticker_info, ()))])
}

fn qmt_single_ticker_message(capability: &str) -> String {
    format!("QMT {capability} currently supports one live ticker at a time")
}

pub fn connect_depth_stream(
    ticker_info: TickerInfo,
    push_freq: PushFrequency,
) -> impl Stream<Item = Event> {
    channel(100, move |mut output| async move {
        let exchange = ticker_info.exchange();
        let stream_kind = StreamKind::Depth {
            ticker_info,
            depth_aggr: StreamTicksize::Client,
            push_freq,
        };
        let mut state = State::Disconnected;
        let mut depth_cache = LocalDepthCache::default();

        loop {
            match &mut state {
                State::Disconnected => {
                    let (domain, url) = match qmt_bridge_ws_url(
                        "/ws/tick",
                        &[("symbol", ticker_info.ticker.to_string())],
                    ) {
                        Ok(parts) => parts,
                        Err(error) => {
                            let _ = output
                                .send(Event::Disconnected(exchange, error.to_string()))
                                .await;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    };

                    match connect_ws(&domain, &url).await {
                        Ok(websocket) => {
                            state = State::Connected(websocket);
                            let _ = output.send(Event::Connected(exchange)).await;
                        }
                        Err(error) => {
                            let _ = output
                                .send(Event::Disconnected(exchange, error.to_string()))
                                .await;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
                State::Connected(ws) => match ws.read_frame().await {
                    Ok(message) => match message.opcode {
                        OpCode::Text => {
                            match serde_json::from_slice::<BridgeWsMessage>(&message.payload[..]) {
                                Ok(BridgeWsMessage::Tick(tick)) => {
                                    cache_live_tick(ticker_info.ticker, &tick);
                                    let payload = tick_to_depth_payload(&tick);
                                    depth_cache.update(
                                        DepthUpdate::Snapshot(payload),
                                        ticker_info.min_ticksize,
                                    );
                                    let _ = output
                                        .send(Event::DepthReceived(
                                            stream_kind,
                                            tick.time,
                                            depth_cache.depth.clone(),
                                        ))
                                        .await;
                                }
                                Ok(BridgeWsMessage::Status(status))
                                    if status.error.is_some()
                                        || status.phase.as_deref() == Some("callback_error") =>
                                {
                                    state = State::Disconnected;
                                    let message =
                                        status.error.or(status.phase).unwrap_or_else(|| {
                                            "QMT bridge reported an error".to_string()
                                        });
                                    let _ =
                                        output.send(Event::Disconnected(exchange, message)).await;
                                }
                                Ok(BridgeWsMessage::Status(_)) => {}
                                Err(error) => {
                                    state = State::Disconnected;
                                    let _ = output
                                        .send(Event::Disconnected(
                                            exchange,
                                            format!("Invalid QMT tick payload: {error}"),
                                        ))
                                        .await;
                                }
                            }
                        }
                        OpCode::Close => {
                            state = State::Disconnected;
                            let _ = output
                                .send(Event::Disconnected(
                                    exchange,
                                    "QMT websocket closed".to_string(),
                                ))
                                .await;
                        }
                        _ => {}
                    },
                    Err(error) => {
                        state = State::Disconnected;
                        let _ = output
                            .send(Event::Disconnected(
                                exchange,
                                format!("QMT websocket read failed: {error}"),
                            ))
                            .await;
                    }
                },
            }
        }
    })
}

pub fn connect_trade_stream(
    tickers: Vec<TickerInfo>,
    _market_type: super::MarketKind,
) -> impl Stream<Item = Event> {
    channel(100, move |mut output| async move {
        let Some(ticker_info) = tickers.first().copied() else {
            return;
        };
        let exchange = ticker_info.exchange();

        if tickers.len() != 1 {
            let _ = output
                .send(Event::Disconnected(
                    exchange,
                    qmt_single_ticker_message("trade stream"),
                ))
                .await;
            return;
        }

        let mut state = State::Disconnected;
        let mut trade_state = SyntheticTradeState::default();
        let ticker_info_map = trade_flush_map(ticker_info);
        let mut trade_buffers = FxHashMap::from_iter([(ticker_info.ticker, Vec::<Trade>::new())]);
        let mut last_flush = tokio::time::Instant::now();

        loop {
            match &mut state {
                State::Disconnected => {
                    let (domain, url) = match qmt_bridge_ws_url(
                        "/ws/tick",
                        &[("symbol", ticker_info.ticker.to_string())],
                    ) {
                        Ok(parts) => parts,
                        Err(error) => {
                            let _ = output
                                .send(Event::Disconnected(exchange, error.to_string()))
                                .await;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    };

                    match connect_ws(&domain, &url).await {
                        Ok(websocket) => {
                            state = State::Connected(websocket);
                            trade_state = SyntheticTradeState::default();
                            last_flush = tokio::time::Instant::now();
                            let _ = output.send(Event::Connected(exchange)).await;
                        }
                        Err(error) => {
                            let _ = output
                                .send(Event::Disconnected(exchange, error.to_string()))
                                .await;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
                State::Connected(ws) => match ws.read_frame().await {
                    Ok(message) => match message.opcode {
                        OpCode::Text => {
                            match serde_json::from_slice::<BridgeWsMessage>(&message.payload[..]) {
                                Ok(BridgeWsMessage::Tick(tick)) => {
                                    let day = china_trading_day(tick.time);
                                    cache_live_tick(ticker_info.ticker, &tick);
                                    let history_ready = day.is_some_and(|day| {
                                        current_day_history_ready(ticker_info.ticker, day)
                                    });

                                    if let Some(trade) = trade_state.update(tick, ticker_info)
                                        && history_ready
                                    {
                                        trade_buffers
                                            .entry(ticker_info.ticker)
                                            .or_default()
                                            .push(trade);
                                    }

                                    if last_flush.elapsed() >= super::TRADE_BUCKET_INTERVAL {
                                        flush_trade_buffers(
                                            &mut output,
                                            &ticker_info_map,
                                            &mut trade_buffers,
                                        )
                                        .await;
                                        last_flush = tokio::time::Instant::now();
                                    }
                                }
                                Ok(BridgeWsMessage::Status(status))
                                    if status.error.is_some()
                                        || status.phase.as_deref() == Some("callback_error") =>
                                {
                                    state = State::Disconnected;
                                    flush_trade_buffers(
                                        &mut output,
                                        &ticker_info_map,
                                        &mut trade_buffers,
                                    )
                                    .await;
                                    let message =
                                        status.error.or(status.phase).unwrap_or_else(|| {
                                            "QMT bridge reported an error".to_string()
                                        });
                                    let _ =
                                        output.send(Event::Disconnected(exchange, message)).await;
                                }
                                Ok(BridgeWsMessage::Status(_)) => {}
                                Err(error) => {
                                    state = State::Disconnected;
                                    let _ = output
                                        .send(Event::Disconnected(
                                            exchange,
                                            format!("Invalid QMT tick payload: {error}"),
                                        ))
                                        .await;
                                }
                            }
                        }
                        OpCode::Close => {
                            flush_trade_buffers(&mut output, &ticker_info_map, &mut trade_buffers)
                                .await;
                            state = State::Disconnected;
                            let _ = output
                                .send(Event::Disconnected(
                                    exchange,
                                    "QMT websocket closed".to_string(),
                                ))
                                .await;
                        }
                        _ => {}
                    },
                    Err(error) => {
                        flush_trade_buffers(&mut output, &ticker_info_map, &mut trade_buffers)
                            .await;
                        state = State::Disconnected;
                        let _ = output
                            .send(Event::Disconnected(
                                exchange,
                                format!("QMT websocket read failed: {error}"),
                            ))
                            .await;
                    }
                },
            }
        }
    })
}

pub fn connect_kline_stream(
    streams: Vec<(TickerInfo, Timeframe)>,
    _market_type: super::MarketKind,
) -> impl Stream<Item = Event> {
    channel(100, move |mut output| async move {
        let Some((ticker_info, _)) = streams.first().copied() else {
            return;
        };
        let exchange = ticker_info.exchange();

        if streams.is_empty() {
            return;
        }

        let unique_tickers = streams
            .iter()
            .map(|(ticker_info, _)| ticker_info.ticker)
            .collect::<HashSet<_>>();
        if unique_tickers.len() != 1 {
            let _ = output
                .send(Event::Disconnected(
                    exchange,
                    qmt_single_ticker_message("kline stream"),
                ))
                .await;
            return;
        }

        let live_streams = streams
            .iter()
            .copied()
            .map(|(ticker_info, timeframe)| LiveKlineStream {
                ticker_info,
                timeframe,
            })
            .collect::<Vec<_>>();

        let mut state = State::Disconnected;

        loop {
            match &mut state {
                State::Disconnected => {
                    let (domain, url) = match qmt_bridge_ws_url(
                        "/ws/tick",
                        &[("symbol", ticker_info.ticker.to_string())],
                    ) {
                        Ok(parts) => parts,
                        Err(error) => {
                            let _ = output
                                .send(Event::Disconnected(exchange, error.to_string()))
                                .await;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    };

                    match connect_ws(&domain, &url).await {
                        Ok(websocket) => {
                            state = State::Connected(websocket);
                            let _ = output.send(Event::Connected(exchange)).await;
                        }
                        Err(error) => {
                            let _ = output
                                .send(Event::Disconnected(exchange, error.to_string()))
                                .await;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
                State::Connected(ws) => match ws.read_frame().await {
                    Ok(message) => match message.opcode {
                        OpCode::Text => {
                            match serde_json::from_slice::<BridgeWsMessage>(&message.payload[..]) {
                                Ok(BridgeWsMessage::Tick(tick)) => {
                                    let day = china_trading_day(tick.time);
                                    cache_live_tick(ticker_info.ticker, &tick);
                                    let history_ready = day.is_some_and(|day| {
                                        current_day_history_ready(ticker_info.ticker, day)
                                    });

                                    if !history_ready {
                                        continue;
                                    }

                                    for stream in &live_streams {
                                        if let Some(kline) = build_live_kline_snapshot(
                                            stream.ticker_info,
                                            stream.timeframe,
                                            &tick,
                                        ) {
                                            let _ = output
                                                .send(Event::KlineReceived(
                                                    StreamKind::Kline {
                                                        ticker_info: stream.ticker_info,
                                                        timeframe: stream.timeframe,
                                                    },
                                                    kline,
                                                ))
                                                .await;
                                        }
                                    }
                                }
                                Ok(BridgeWsMessage::Status(status))
                                    if status.error.is_some()
                                        || status.phase.as_deref() == Some("callback_error") =>
                                {
                                    state = State::Disconnected;
                                    let message =
                                        status.error.or(status.phase).unwrap_or_else(|| {
                                            "QMT bridge reported an error".to_string()
                                        });
                                    let _ =
                                        output.send(Event::Disconnected(exchange, message)).await;
                                }
                                Ok(BridgeWsMessage::Status(_)) => {}
                                Err(error) => {
                                    state = State::Disconnected;
                                    let _ = output
                                        .send(Event::Disconnected(
                                            exchange,
                                            format!("Invalid QMT tick payload: {error}"),
                                        ))
                                        .await;
                                }
                            }
                        }
                        OpCode::Close => {
                            state = State::Disconnected;
                            let _ = output
                                .send(Event::Disconnected(
                                    exchange,
                                    "QMT websocket closed".to_string(),
                                ))
                                .await;
                        }
                        _ => {}
                    },
                    Err(error) => {
                        state = State::Disconnected;
                        let _ = output
                            .send(Event::Disconnected(
                                exchange,
                                format!("QMT websocket read failed: {error}"),
                            ))
                            .await;
                    }
                },
            }
        }
    })
}

pub async fn fetch_ticker_metadata(
    _venue: Venue,
) -> Result<HashMap<Ticker, Option<TickerInfo>>, AdapterError> {
    // QMT has a very large stock universe and only supports a small number of
    // active live subscriptions, so we do not pre-load venue-wide metadata.
    Ok(HashMap::new())
}

pub async fn search_ticker_metadata(
    venue: Venue,
    query: &str,
    limit: usize,
) -> Result<HashMap<Ticker, Option<TickerInfo>>, AdapterError> {
    if query.trim().is_empty() {
        return Ok(HashMap::new());
    }

    let venue_name = venue.to_string();
    let url = qmt_bridge_http_url(
        "/api/v1/search",
        &[
            ("venue", venue_name),
            ("query", query.trim().to_string()),
            ("limit", limit.max(1).to_string()),
        ],
    )?;
    let response = reqwest::get(&url).await.map_err(AdapterError::from)?;
    let status = response.status();
    let text = response.text().await.map_err(AdapterError::from)?;

    if !status.is_success() {
        return Err(AdapterError::http_status_failed(
            status,
            format!("GET {url} failed: {text}"),
        ));
    }

    let parsed: BridgeItemsResponse<BridgeSearchItem> =
        serde_json::from_str(&text).map_err(|e| AdapterError::ParseError(e.to_string()))?;
    let mut map = HashMap::new();
    for item in parsed.items {
        let _ = item.display_name.as_deref();
        let Some(exchange) = qmt_exchange_from_symbol(&item.symbol) else {
            continue;
        };
        if exchange.venue() != venue {
            continue;
        }
        let ticker = Ticker::new(&item.symbol, exchange);
        map.insert(
            ticker,
            Some(TickerInfo::new(
                ticker,
                item.min_ticksize,
                item.min_qty,
                None,
            )),
        );
    }

    Ok(map)
}

pub async fn fetch_ticker_stats(
    _venue: Venue,
) -> Result<HashMap<Ticker, TickerStats>, AdapterError> {
    Ok(HashMap::new())
}

pub async fn fetch_klines(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(u64, u64)>,
) -> Result<Vec<Kline>, AdapterError> {
    Ok(fetch_tick_derived_history(ticker_info, timeframe, range, false)
        .await?
        .0)
}

pub async fn fetch_klines_and_trades(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(u64, u64)>,
) -> Result<(Vec<Kline>, Vec<Trade>), AdapterError> {
    fetch_tick_derived_history(ticker_info, timeframe, range, true).await
}

pub async fn historical_day_ranges(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(u64, u64)>,
) -> Result<Vec<(u64, u64)>, AdapterError> {
    let (requested_start, requested_end) = range
        .or_else(|| qmt_default_kline_range(timeframe))
        .ok_or_else(|| {
            AdapterError::InvalidRequest(format!(
                "unsupported QMT timeframe for historical klines: {timeframe}"
            ))
        })?;

    let venue = ticker_info.exchange().venue();
    let calendar_seed_start = requested_start.saturating_sub(QMT_KLINE_SEED_CALENDAR_LOOKBACK_MS);
    if let Err(error) = ensure_trading_calendar(venue, calendar_seed_start, requested_end).await {
        log::warn!(
            "QMT trading calendar seed fetch failed for {}: {error}",
            ticker_info.ticker
        );
    }

    let Some((start_day, end_day)) =
        trading_day_range_from_timestamps(requested_start, requested_end)
    else {
        return Ok(Vec::new());
    };

    let mut day_ranges = Vec::new();
    for day in qmt_trading_days_between(venue, start_day, end_day).into_iter().rev() {
        let Some((day_start, day_end)) = qmt_tick_fetch_bounds(venue, day) else {
            continue;
        };

        if requested_end > day_start && requested_start < day_end {
            if current_china_day() == Some(day) {
                if let Some(range) = qmt_current_day_history_bounds(day) {
                    day_ranges.push(range);
                }
            } else {
                day_ranges.push((day_start, day_end));
            }
        }
    }

    Ok(day_ranges)
}

async fn fetch_tick_derived_history(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(u64, u64)>,
    latest_day_only: bool,
) -> Result<(Vec<Kline>, Vec<Trade>), AdapterError> {
    let total_started_at = Instant::now();
    let (requested_start, requested_end) = range
        .or_else(|| qmt_default_kline_range(timeframe))
        .ok_or_else(|| {
            AdapterError::InvalidRequest(format!(
                "unsupported QMT timeframe for historical klines: {timeframe}"
            ))
        })?;

    let venue = ticker_info.exchange().venue();
    let calendar_seed_start = requested_start.saturating_sub(QMT_KLINE_SEED_CALENDAR_LOOKBACK_MS);
    if let Err(error) = ensure_trading_calendar(venue, calendar_seed_start, requested_end).await {
        log::warn!(
            "QMT trading calendar seed fetch failed for {}: {error}",
            ticker_info.ticker
        );
    }

    let (start, end) = if latest_day_only {
        let chunk = qmt_latest_history_chunk_range(venue, requested_start, requested_end)
            .unwrap_or((requested_start, requested_end));
        if chunk != (requested_start, requested_end) {
            log::info!(
                "QMT combined history for {} clamped from {}..{} to latest trading-day chunk {}..{}",
                ticker_info.ticker,
                requested_start,
                requested_end,
                chunk.0,
                chunk.1
            );
        }
        chunk
    } else {
        (requested_start, requested_end)
    };

    let tick_fetch_start = qmt_kline_seed_start(venue, start).unwrap_or(start);
    let fetch_started_at = Instant::now();
    let ticks = fetch_ticks(ticker_info, (tick_fetch_start, end)).await?;
    let fetch_elapsed = fetch_started_at.elapsed();
    let derive_started_at = Instant::now();
    let trades = synthesize_trades_from_ticks(&ticks, ticker_info);
    let klines = aggregate_trades_to_klines(&trades, &ticks, ticker_info, timeframe, start, end)?;
    let filtered_trades: Vec<Trade> = trades
        .into_iter()
        .filter(|trade| start <= trade.time && trade.time <= end)
        .collect();
    let derive_elapsed = derive_started_at.elapsed();

    log::warn!(
        "QMT derived history {} {} latest_day_only={} requested={:?} effective=({}..{}) seed_start={} ticks={} trades={} klines={} fetch_elapsed={:?} derive_elapsed={:?} total_elapsed={:?}",
        ticker_info.ticker,
        timeframe,
        latest_day_only,
        range,
        start,
        end,
        tick_fetch_start,
        ticks.len(),
        filtered_trades.len(),
        klines.len(),
        fetch_elapsed,
        derive_elapsed,
        total_started_at.elapsed()
    );

    Ok((klines, filtered_trades))
}

pub async fn fetch_trades(
    ticker_info: TickerInfo,
    range: (u64, u64),
) -> Result<Vec<Trade>, AdapterError> {
    let (start, end) = range;
    let ticks = fetch_ticks(ticker_info, (start, end)).await?;
    Ok(synthesize_trades_from_ticks(&ticks, ticker_info)
        .into_iter()
        .filter(|trade| start <= trade.time && trade.time <= end)
        .collect())
}

async fn fetch_ticks(
    ticker_info: TickerInfo,
    range: (u64, u64),
) -> Result<Vec<QmtTick>, AdapterError> {
    let (start, end) = range;
    let venue = ticker_info.exchange().venue();
    if let Err(error) = ensure_trading_calendar(venue, start, end).await {
        log::warn!(
            "QMT trading calendar fetch failed for {}: {error}",
            ticker_info.ticker
        );
    }

    let Some((start_day, end_day)) = trading_day_range_from_timestamps(start, end) else {
        return Ok(Vec::new());
    };
    let trading_days = qmt_trading_days_between(venue, start_day, end_day);

    if trading_days.is_empty() {
        return Ok(Vec::new());
    }

    if trading_days.len() > 1 {
        log::info!(
            "QMT historical tick fetch for {} spans {} trading days",
            ticker_info.ticker,
            trading_days.len()
        );
    }

    let mut ticks = Vec::new();
    for trading_day in trading_days {
        let mut chunk = if current_china_day() == Some(trading_day) {
            fetch_current_day_ticks(ticker_info, trading_day).await?
        } else {
            fetch_tick_day(ticker_info, trading_day).await?
        };
        ticks.append(&mut chunk);
    }

    Ok(ticks)
}

async fn fetch_tick_day(ticker_info: TickerInfo, day: NaiveDate) -> Result<Vec<QmtTick>, AdapterError> {
    if let Some(error) = recent_tick_fetch_failure(ticker_info.ticker, day) {
        return Err(AdapterError::InvalidRequest(format!(
            "QMT historical tick fetch cooling down for {} on {} after previous failure: {}",
            ticker_info.ticker, day, error
        )));
    }

    if let Some(cached_ticks) = get_cached_tick_day(ticker_info.ticker, day) {
        return Ok(cached_ticks);
    }

    let Some(range) = qmt_tick_fetch_bounds(ticker_info.exchange().venue(), day) else {
        return Ok(Vec::new());
    };

    let ticks = match fetch_tick_chunk(ticker_info, range).await {
        Ok(ticks) => ticks,
        Err(error) => {
            cache_tick_fetch_failure(ticker_info.ticker, day, &error);
            return Err(error);
        }
    };
    clear_tick_fetch_failure(ticker_info.ticker, day);
    cache_tick_day(ticker_info.ticker, day, ticks.clone());
    Ok(ticks)
}

async fn fetch_current_day_ticks(
    ticker_info: TickerInfo,
    day: NaiveDate,
) -> Result<Vec<QmtTick>, AdapterError> {
    if let Some(error) = recent_tick_fetch_failure(ticker_info.ticker, day) {
        return Err(AdapterError::InvalidRequest(format!(
            "QMT current-day tick fetch cooling down for {} on {} after previous failure: {}",
            ticker_info.ticker, day, error
        )));
    }

    let Some(range) = qmt_current_day_history_bounds(day) else {
        return Ok(Vec::new());
    };

    let history_ticks = match fetch_tick_chunk(ticker_info, range).await {
        Ok(ticks) => ticks,
        Err(error) => {
            cache_tick_fetch_failure(ticker_info.ticker, day, &error);
            return Err(error);
        }
    };
    clear_tick_fetch_failure(ticker_info.ticker, day);
    Ok(merge_current_day_history_and_live(
        ticker_info.ticker,
        day,
        history_ticks,
    ))
}

async fn fetch_tick_chunk(
    ticker_info: TickerInfo,
    range: (u64, u64),
) -> Result<Vec<QmtTick>, AdapterError> {
    let (start, end) = range;
    let url = qmt_bridge_http_url(
        "/api/v1/ticks",
        &[
            ("symbol", ticker_info.ticker.to_string()),
            ("start", start.to_string()),
            ("end", end.to_string()),
        ],
    )?;

    let response = reqwest::get(&url).await.map_err(AdapterError::from)?;
    let status = response.status();
    let text = response.text().await.map_err(AdapterError::from)?;

    if !status.is_success() {
        return Err(AdapterError::http_status_failed(
            status,
            format!("GET {url} failed: {text}"),
        ));
    }

    let parsed: BridgeItemsResponse<QmtTick> =
        serde_json::from_str(&text).map_err(|e| AdapterError::ParseError(e.to_string()))?;
    Ok(parsed.items)
}

pub async fn fetch_historical_oi(
    _ticker_info: TickerInfo,
    _range: Option<(u64, u64)>,
    _period: Timeframe,
) -> Result<Vec<OpenInterest>, AdapterError> {
    Ok(vec![])
}
