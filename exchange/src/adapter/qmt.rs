use crate::{
    Kline, OpenInterest, Price, PushFrequency, Ticker, TickerInfo, TickerStats, Timeframe, Trade,
    Volume,
    adapter::{AdapterError, Event, StreamKind, StreamTicksize, Venue, flush_trade_buffers},
    connect::{channel, connect_ws},
    depth::{DeOrder, Depth, DepthPayload, DepthUpdate, LocalDepthCache},
    order::{
        OrderCancelRequest, OrderCancelResponse, OrderPanelSnapshot, OrderSubmitRequest,
        OrderSubmitResponse,
    },
    unit::Qty,
};

use chrono::{Datelike, Days, FixedOffset, NaiveDate, TimeZone};
use fastwebsockets::OpCode;
use futures::Stream;
use indexmap::IndexMap;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    env,
    sync::{
        LazyLock, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use url::Url;

mod api;
mod cache;
mod history;
mod shared;
mod streams;
#[cfg(test)]
mod tests;
mod time;

pub use api::{
    cancel_order, fetch_heatmap_history, fetch_historical_oi, fetch_klines,
    fetch_klines_and_trades, fetch_order_panel_snapshot, fetch_ticker_metadata, fetch_ticker_stats,
    fetch_trades, historical_day_ranges, search_ticker_metadata, submit_order,
};
use cache::*;
use history::*;
use shared::*;
use streams::build_depth_history_from_ticks;
pub use streams::{connect_depth_stream, connect_kline_stream, connect_trade_stream};
use time::*;
pub use time::{
    is_trading_bucket_start, supports_gapless_time_axis_timeframe, time_axis_bucket_at_offset,
    time_axis_bucket_offset, uses_gapless_time_axis,
};

const DEFAULT_QMT_BRIDGE_BASE: &str = "http://127.0.0.1:8765";
const DEFAULT_QMT_INITIAL_KLINE_BARS: u64 = 450;
const QMT_VOLUME_LOT_SIZE: f32 = 100.0;
const QMT_KLINE_SEED_CALENDAR_LOOKBACK_MS: u64 = 14 * 86_400_000;
const QMT_CLOSE_BUCKET_GRACE_MS: u64 = 5_000;
const QMT_TICK_CACHE_MAX_DAYS_PER_SYMBOL: usize = 32;
const QMT_TICK_FETCH_FAILURE_COOLDOWN: Duration = Duration::from_secs(60);
const QMT_SYNTHETIC_WARN_SAMPLE_LIMIT: usize = 5;

static TRADING_DAY_CACHE: LazyLock<RwLock<HashMap<Venue, TradingDayCache>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static TICK_DAY_CACHE: LazyLock<RwLock<QmtTickDayCache>> =
    LazyLock::new(|| RwLock::new(QmtTickDayCache::new(QMT_TICK_CACHE_MAX_DAYS_PER_SYMBOL)));
static CURRENT_DAY_TICK_CACHE: LazyLock<RwLock<FxHashMap<Ticker, CurrentDayTickCacheEntry>>> =
    LazyLock::new(|| RwLock::new(FxHashMap::default()));
static TICK_FETCH_FAILURE_CACHE: LazyLock<
    RwLock<FxHashMap<(Ticker, NaiveDate), TickFetchFailureEntry>>,
> = LazyLock::new(|| RwLock::new(FxHashMap::default()));
static INVALID_TOP_OF_BOOK_WARN_COUNT: AtomicUsize = AtomicUsize::new(0);
static VOLUME_REGRESSION_WARN_COUNT: AtomicUsize = AtomicUsize::new(0);
static MISSING_LAST_PRICE_WARN_COUNT: AtomicUsize = AtomicUsize::new(0);
static ZERO_QTY_WARN_COUNT: AtomicUsize = AtomicUsize::new(0);

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
    last_price: f32,
    open: f32,
    high: f32,
    low: f32,
    last_close: f32,
    volume: u64,
    ask_price: Vec<f32>,
    bid_price: Vec<f32>,
    ask_vol: Vec<f32>,
    bid_vol: Vec<f32>,
}

impl QmtTick {
    fn valid_last_price(&self) -> Option<f32> {
        positive_f32(self.last_price)
    }

    fn valid_open(&self) -> Option<f32> {
        positive_f32(self.open)
    }

    fn valid_high(&self) -> Option<f32> {
        positive_f32(self.high)
    }

    fn valid_low(&self) -> Option<f32> {
        positive_f32(self.low)
    }

    fn valid_last_close(&self) -> Option<f32> {
        positive_f32(self.last_close)
    }

    fn valid_bid1(&self) -> Option<f32> {
        self.bid_price.first().copied().and_then(positive_f32)
    }

    fn valid_ask1(&self) -> Option<f32> {
        self.ask_price.first().copied().and_then(positive_f32)
    }
}

fn positive_f32(value: f32) -> Option<f32> {
    (value > 0.0).then_some(value)
}

fn log_qmt_synthetic_warning(
    counter: &AtomicUsize,
    category: &str,
    details: impl FnOnce() -> String,
) {
    let seen = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if seen <= QMT_SYNTHETIC_WARN_SAMPLE_LIMIT {
        log::warn!(
            "QMT synthetic trade {category} [sample {seen}/{}]: {}",
            QMT_SYNTHETIC_WARN_SAMPLE_LIMIT,
            details()
        );
    } else if seen == QMT_SYNTHETIC_WARN_SAMPLE_LIMIT + 1 {
        log::warn!(
            "QMT synthetic trade {category}: further warnings suppressed after {} samples",
            QMT_SYNTHETIC_WARN_SAMPLE_LIMIT
        );
    }
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeOrderSubmitRequest<'a> {
    symbol: &'a str,
    side: crate::order::OrderSide,
    order_type: crate::order::OrderType,
    price: Option<f32>,
    quantity: f32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeOrderCancelRequest<'a> {
    symbol: &'a str,
    order_id: &'a str,
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
    last_history_loaded_at: Option<Instant>,
    history_depth_seed: Option<Depth>,
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
    #[allow(dead_code)]
    display_name: Option<String>,
    min_ticksize: f32,
    min_qty: f32,
}

#[derive(Default)]
struct SyntheticTradeState {
    previous_tick: Option<QmtTick>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyntheticTradeSide {
    Buy,
    Sell,
    Split,
}

#[derive(Debug, Default)]
struct TradingDayCache {
    covered_ranges: Vec<(NaiveDate, NaiveDate)>,
    trading_days: HashSet<NaiveDate>,
}
