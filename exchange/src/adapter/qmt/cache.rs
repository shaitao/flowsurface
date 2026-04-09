use super::*;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard};

static CURRENT_DAY_FETCH_LOCKS: LazyLock<Mutex<FxHashMap<(Ticker, NaiveDate), Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(FxHashMap::default()));

impl QmtTickDayCache {
    pub(super) fn new(max_days_per_symbol: usize) -> Self {
        Self {
            max_days_per_symbol,
            entries: FxHashMap::default(),
        }
    }

    pub(super) fn get(&mut self, ticker: Ticker, day: NaiveDate) -> Option<Vec<QmtTick>> {
        let day_cache = self.entries.get_mut(&ticker)?;
        let ticks = day_cache.shift_remove(&day)?;
        let cached = ticks.clone();
        day_cache.insert(day, ticks);
        Some(cached)
    }

    pub(super) fn insert(&mut self, ticker: Ticker, day: NaiveDate, ticks: Vec<QmtTick>) {
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
    pub(super) fn update(&mut self, current_tick: QmtTick, ticker_info: TickerInfo) -> Vec<Trade> {
        if !qmt_tick_has_traded_volume(&current_tick) {
            if self
                .previous_tick
                .as_ref()
                .is_none_or(|tick| !qmt_tick_has_traded_volume(tick))
            {
                self.previous_tick = Some(current_tick);
            }
            return Vec::new();
        }
        let trades = self
            .previous_tick
            .as_ref()
            .map(|previous_tick| {
                synthesize_trades_for_tick_pair(previous_tick, &current_tick, ticker_info)
            })
            .unwrap_or_default();
        self.previous_tick = Some(current_tick);
        trades
    }
}

pub(super) fn qmt_exchange_from_symbol(symbol: &str) -> Option<super::super::Exchange> {
    if symbol.ends_with(".SH") {
        Some(super::super::Exchange::SSH)
    } else if symbol.ends_with(".SZ") {
        Some(super::super::Exchange::SSZ)
    } else {
        None
    }
}

pub(super) fn is_weekday(day: NaiveDate) -> bool {
    day.weekday().num_days_from_monday() < 5
}

pub(super) fn trading_day_range_from_timestamps(
    start_ms: u64,
    end_ms: u64,
) -> Option<(NaiveDate, NaiveDate)> {
    if end_ms < start_ms {
        return None;
    }
    Some((
        china_datetime(start_ms)?.date_naive(),
        china_datetime(end_ms)?.date_naive(),
    ))
}

pub(super) fn merge_trading_day_range(
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

pub(super) fn trading_day_range_is_cached(
    venue: Venue,
    start_day: NaiveDate,
    end_day: NaiveDate,
) -> bool {
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

pub(super) fn cache_trading_days(
    venue: Venue,
    start_day: NaiveDate,
    end_day: NaiveDate,
    days: &[NaiveDate],
) {
    let Ok(mut cache) = TRADING_DAY_CACHE.write() else {
        return;
    };
    let entry = cache.entry(venue).or_default();
    entry.trading_days.extend(days.iter().copied());
    merge_trading_day_range(&mut entry.covered_ranges, start_day, end_day);
}

pub(super) fn get_cached_tick_day(ticker: Ticker, day: NaiveDate) -> Option<Vec<QmtTick>> {
    let Ok(mut cache) = TICK_DAY_CACHE.write() else {
        return None;
    };
    cache.get(ticker, day)
}

pub(super) fn cache_tick_day(ticker: Ticker, day: NaiveDate, ticks: Vec<QmtTick>) {
    let Ok(mut cache) = TICK_DAY_CACHE.write() else {
        return;
    };
    cache.insert(ticker, day, ticks);
}

pub(super) fn qmt_tick_has_traded_volume(tick: &QmtTick) -> bool {
    tick.volume > 0
}

pub(super) fn qmt_tick_has_top_of_book(tick: &QmtTick) -> bool {
    tick.valid_bid1().is_some()
        && tick.valid_ask1().is_some()
        && tick.bid_vol.first().is_some_and(|qty| *qty > 0.0)
        && tick.ask_vol.first().is_some_and(|qty| *qty > 0.0)
}

pub(super) fn qmt_synthetic_warning_enabled(
    previous_tick: &QmtTick,
    current_tick: &QmtTick,
) -> bool {
    qmt_tick_has_traded_volume(previous_tick) && qmt_tick_has_traded_volume(current_tick)
}

pub(super) fn sanitize_qmt_ticks(mut ticks: Vec<QmtTick>) -> Vec<QmtTick> {
    if ticks.is_empty() {
        return ticks;
    }

    ticks.sort_by_key(|tick| tick.time);

    let mut sanitized = Vec::with_capacity(ticks.len());
    let mut pending_zero_tick = None;
    let mut current_day = None;
    let mut seen_traded_volume = false;

    for tick in ticks {
        let day = china_trading_day(tick.time);
        if day != current_day {
            pending_zero_tick = None;
            current_day = day;
            seen_traded_volume = false;
        }

        if qmt_tick_has_traded_volume(&tick) {
            if !seen_traded_volume {
                if let Some(zero_tick) = pending_zero_tick.take() {
                    sanitized.push(zero_tick);
                }
                seen_traded_volume = true;
            }
            sanitized.push(tick);
            continue;
        }

        if !seen_traded_volume {
            pending_zero_tick = Some(tick);
        }
    }

    sanitized
}

pub(super) fn merge_ticks(
    existing: &[QmtTick],
    incoming: impl IntoIterator<Item = QmtTick>,
) -> Vec<QmtTick> {
    let mut by_timestamp = FxHashMap::default();

    for tick in existing.iter().cloned() {
        by_timestamp.insert(tick.time, tick);
    }
    for tick in incoming {
        by_timestamp.insert(tick.time, tick);
    }

    sanitize_qmt_ticks(by_timestamp.into_values().collect())
}

pub(super) fn prune_current_day_tick_cache(
    cache: &mut FxHashMap<Ticker, CurrentDayTickCacheEntry>,
) {
    let Some(today) = current_china_day() else {
        cache.clear();
        return;
    };
    cache.retain(|_, entry| entry.day == today);
}

pub(super) fn cache_live_tick(ticker: Ticker, tick: &QmtTick) {
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
            last_history_loaded_at: None,
            history_depth_seed: None,
        });

    if entry.day != day {
        *entry = CurrentDayTickCacheEntry {
            day,
            ticks: Vec::new(),
            history_loaded: false,
            last_history_loaded_at: None,
            history_depth_seed: None,
        };
    }

    entry.ticks = merge_ticks(&entry.ticks, std::iter::once(tick.clone()));
}

fn build_history_depth_seed_from_ticks(
    ticker_info: TickerInfo,
    ticks: &[QmtTick],
) -> Option<Depth> {
    build_depth_history_from_ticks(
        ticks,
        ticker_info,
        Some(crate::QMT_SYNTHETIC_BOOK_LEVELS_MAX),
    )
    .into_iter()
    .last()
    .map(|(_, depth)| depth)
}

pub(super) fn merge_current_day_history_and_live(
    ticker_info: TickerInfo,
    day: NaiveDate,
    history_ticks: Vec<QmtTick>,
) -> Vec<QmtTick> {
    let ticker = ticker_info.ticker;
    let history_depth_seed = build_history_depth_seed_from_ticks(ticker_info, &history_ticks);

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
            last_history_loaded_at: None,
            history_depth_seed: None,
        });

    if entry.day != day {
        *entry = CurrentDayTickCacheEntry {
            day,
            ticks: Vec::new(),
            history_loaded: false,
            last_history_loaded_at: None,
            history_depth_seed: None,
        };
    }

    entry.ticks = merge_ticks(&history_ticks, entry.ticks.clone());
    entry.history_loaded = true;
    entry.last_history_loaded_at = Some(Instant::now());
    entry.history_depth_seed = history_depth_seed;
    entry.ticks.clone()
}

pub(super) fn current_day_tick_snapshot(ticker: Ticker, day: NaiveDate) -> Option<Vec<QmtTick>> {
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

pub(super) fn current_day_history_snapshot_if_fresh(
    ticker: Ticker,
    day: NaiveDate,
    max_age: Duration,
) -> Option<Vec<QmtTick>> {
    let Ok(mut cache) = CURRENT_DAY_TICK_CACHE.write() else {
        return None;
    };
    prune_current_day_tick_cache(&mut cache);

    let entry = cache.get(&ticker)?;
    if entry.day != day || !entry.history_loaded {
        return None;
    }

    let loaded_at = entry.last_history_loaded_at?;
    if loaded_at.elapsed() > max_age {
        return None;
    }

    Some(entry.ticks.clone())
}

pub(super) fn current_day_history_ready(ticker: Ticker, day: NaiveDate) -> bool {
    let Ok(mut cache) = CURRENT_DAY_TICK_CACHE.write() else {
        return false;
    };
    prune_current_day_tick_cache(&mut cache);
    cache
        .get(&ticker)
        .is_some_and(|entry| entry.day == day && entry.history_loaded)
}

pub(super) fn current_day_history_depth_seed(ticker: Ticker, day: NaiveDate) -> Option<Depth> {
    let Ok(mut cache) = CURRENT_DAY_TICK_CACHE.write() else {
        return None;
    };
    prune_current_day_tick_cache(&mut cache);
    let entry = cache.get(&ticker)?;
    if entry.day != day || !entry.history_loaded {
        return None;
    }

    entry.history_depth_seed.clone()
}

pub(super) async fn acquire_current_day_fetch_lock(
    ticker: Ticker,
    day: NaiveDate,
) -> OwnedMutexGuard<()> {
    let key = (ticker, day);
    let lock = {
        let mut locks = CURRENT_DAY_FETCH_LOCKS.lock().await;
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };

    lock.lock_owned().await
}

pub(super) fn build_live_kline_from_ticks(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    latest_tick: &QmtTick,
    ticks: &[QmtTick],
) -> Option<Kline> {
    let venue = ticker_info.exchange().venue();
    let bucket_start = qmt_bucket_start(venue, latest_tick.time, timeframe)?;

    let last_seed_tick = ticks
        .iter()
        .rev()
        .find(|tick| tick.time < bucket_start)
        .cloned();
    let mut bucket_ticks = ticks
        .iter()
        .filter(|tick| bucket_start <= tick.time && tick.time <= latest_tick.time)
        .cloned()
        .collect::<Vec<_>>();

    if bucket_ticks.is_empty() {
        return None;
    }

    let mut relevant_ticks =
        Vec::with_capacity(bucket_ticks.len() + usize::from(last_seed_tick.is_some()));
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

    let first_bucket_tick = relevant_ticks
        .iter()
        .find(|tick| tick.time >= bucket_start)?;
    let close = latest_tick.valid_last_price()?;
    let open = if let Some(day) = china_trading_day(latest_tick.time) {
        let opening_session_start = qmt_session_bounds(venue, day)
            .and_then(|sessions| sessions.first().copied())
            .map(|(session_start, _)| session_start);
        let opening_bucket = opening_session_start
            .and_then(|session_start| qmt_bucket_start(venue, session_start, timeframe));

        if opening_bucket == Some(bucket_start) {
            first_bucket_tick
                .valid_open()
                .or_else(|| first_bucket_tick.valid_last_price())
        } else {
            last_seed_tick
                .as_ref()
                .and_then(|tick| tick.valid_last_price())
                .or_else(|| first_bucket_tick.valid_last_price())
        }
    } else {
        first_bucket_tick.valid_last_price()
    }?;

    let mut high = close;
    let mut low = close;
    for tick in relevant_ticks
        .iter()
        .filter(|tick| tick.time >= bucket_start)
    {
        if let Some(last_price) = tick.valid_last_price() {
            high = high.max(last_price);
            low = low.min(last_price);
        }
    }

    let baseline_volume = last_seed_tick.as_ref().map_or(0, |tick| tick.volume);
    let current_volume =
        latest_tick.volume.saturating_sub(baseline_volume) as f32 * QMT_VOLUME_LOT_SIZE;

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

pub(super) fn build_live_kline_snapshot(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    latest_tick: &QmtTick,
) -> Option<Kline> {
    let day = china_trading_day(latest_tick.time)?;
    let ticks = current_day_tick_snapshot(ticker_info.ticker, day)?;
    build_live_kline_from_ticks(ticker_info, timeframe, latest_tick, &ticks)
}

pub(super) fn recent_tick_fetch_failure(ticker: Ticker, day: NaiveDate) -> Option<String> {
    let Ok(mut cache) = TICK_FETCH_FAILURE_CACHE.write() else {
        return None;
    };

    let now = Instant::now();
    cache.retain(|_, entry| now.duration_since(entry.failed_at) < QMT_TICK_FETCH_FAILURE_COOLDOWN);
    cache.get(&(ticker, day)).map(|entry| entry.error.clone())
}

pub(super) fn cache_tick_fetch_failure(ticker: Ticker, day: NaiveDate, error: &AdapterError) {
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

pub(super) fn clear_tick_fetch_failure(ticker: Ticker, day: NaiveDate) {
    let Ok(mut cache) = TICK_FETCH_FAILURE_CACHE.write() else {
        return;
    };
    cache.remove(&(ticker, day));
}

pub(super) fn is_qmt_trading_day(venue: Venue, day: NaiveDate) -> bool {
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
