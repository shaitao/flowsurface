use super::{Ticker, Timeframe};
use crate::{
    Kline, OpenInterest, Price, PushFrequency, TickMultiplier, TickerInfo, TickerStats, Trade,
    depth::Depth, unit::Qty,
};

use enum_map::{Enum, EnumMap};
use futures::SinkExt;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};

pub mod binance;
pub mod qmt;

/// Buffer trades and flush in this interval
const TRADE_BUCKET_INTERVAL: Duration = Duration::from_micros(33_333);

async fn flush_trade_buffers<V>(
    output: &mut futures::channel::mpsc::Sender<Event>,
    ticker_info_map: &FxHashMap<Ticker, (TickerInfo, V)>,
    trade_buffers_map: &mut FxHashMap<Ticker, Vec<Trade>>,
) {
    let interval_ms = TRADE_BUCKET_INTERVAL.as_millis() as u64;

    for (ticker, trades_buffer) in trade_buffers_map.iter_mut() {
        if trades_buffer.is_empty() {
            continue;
        }

        let bucket_update_t = trades_buffer
            .iter()
            .map(|t| t.time)
            .max()
            .map(|t| (t / interval_ms) * interval_ms);

        if let Some((ticker_info, _)) = ticker_info_map.get(ticker)
            && let Some(update_t) = bucket_update_t
        {
            let _ = output
                .send(Event::TradesReceived(
                    StreamKind::Trades {
                        ticker_info: *ticker_info,
                    },
                    update_t,
                    std::mem::take(trades_buffer).into_boxed_slice(),
                ))
                .await;
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AdapterError {
    #[error("{0}")]
    FetchError(FetchError),
    #[error("Parsing: {0}")]
    ParseError(String),
    #[error("Stream: {0}")]
    WebsocketError(String),
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReqwestErrorKind {
    Timeout,
    Connect,
    Decode,
    Body,
    Request,
    Other,
}

impl ReqwestErrorKind {
    fn from_error(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else if error.is_connect() {
            Self::Connect
        } else if error.is_decode() {
            Self::Decode
        } else if error.is_body() {
            Self::Body
        } else if error.is_request() {
            Self::Request
        } else {
            Self::Other
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Decode => "decode",
            Self::Body => "body",
            Self::Request => "request",
            Self::Other => "other",
        }
    }

    fn ui_message(self) -> &'static str {
        match self {
            Self::Timeout => "Request timed out. Check logs for details.",
            Self::Connect => "Connection failed. Check logs for details.",
            Self::Decode | Self::Body => "Invalid server response. Check logs for details.",
            Self::Request | Self::Other => "Request failed. Check logs for details.",
        }
    }
}

#[derive(Debug)]
pub struct FetchError {
    detail: String,
    ui_message: &'static str,
}

impl FetchError {
    fn from_reqwest_detail(error: &reqwest::Error, detail: String) -> Self {
        let ui_message = ReqwestErrorKind::from_error(error).ui_message();

        Self { detail, ui_message }
    }

    fn from_status_detail(status: reqwest::StatusCode, detail: String) -> Self {
        let ui_message = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            "Rate limited. Check logs for details."
        } else if status.is_server_error() {
            "Server error. Check logs for details."
        } else if status.is_client_error() {
            "Request was rejected. Check logs for details."
        } else {
            "Request failed. Check logs for details."
        };

        Self { detail, ui_message }
    }

    pub fn ui_message(&self) -> &'static str {
        self.ui_message
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

fn format_reqwest_error(error: &reqwest::Error) -> String {
    let kind = ReqwestErrorKind::from_error(error);
    let mut details = vec![error.to_string(), format!("kind={}", kind.as_str())];

    if let Some(status) = error.status() {
        details.push(format!("status={status}"));
    }

    if let Some(url) = error.url() {
        details.push(format!("url={url}"));
    }

    details.join(" | ")
}

impl From<reqwest::Error> for AdapterError {
    fn from(error: reqwest::Error) -> Self {
        let detail = format_reqwest_error(&error);
        Self::FetchError(FetchError::from_reqwest_detail(&error, detail))
    }
}

impl AdapterError {
    pub(crate) fn request_failed(
        method: &reqwest::Method,
        url: &str,
        error: reqwest::Error,
    ) -> Self {
        let detail = format!(
            "{} {}: request failed | {}",
            method,
            url,
            format_reqwest_error(&error)
        );
        Self::FetchError(FetchError::from_reqwest_detail(&error, detail))
    }

    pub(crate) fn response_body_failed(
        method: &reqwest::Method,
        url: &str,
        status: reqwest::StatusCode,
        content_type: &str,
        error: reqwest::Error,
    ) -> Self {
        let detail = format!(
            "{} {}: failed reading response body | status={} | content-type={} | {}",
            method,
            url,
            status,
            content_type,
            format_reqwest_error(&error)
        );
        Self::FetchError(FetchError::from_reqwest_detail(&error, detail))
    }

    pub(crate) fn http_status_failed(status: reqwest::StatusCode, detail: String) -> Self {
        Self::FetchError(FetchError::from_status_detail(status, detail))
    }

    pub fn ui_message(&self) -> String {
        match self {
            Self::FetchError(error) => error.ui_message().to_string(),
            Self::ParseError(_) => "Invalid server response. Check logs for details.".to_string(),
            Self::WebsocketError(_) => "Stream error. Check logs for details.".to_string(),
            Self::InvalidRequest(message) => message.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum MarketKind {
    Spot,
    LinearPerps,
    InversePerps,
}

impl MarketKind {
    pub const ALL: [MarketKind; 3] = [
        MarketKind::Spot,
        MarketKind::LinearPerps,
        MarketKind::InversePerps,
    ];

    pub fn qty_in_quote_value(&self, qty: Qty, price: Price, size_in_quote_ccy: bool) -> f32 {
        let qty = qty.to_f32_lossy();

        match self {
            MarketKind::InversePerps => qty,
            _ => {
                if size_in_quote_ccy {
                    qty
                } else {
                    price.to_f32() * qty
                }
            }
        }
    }
}

impl std::fmt::Display for MarketKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                MarketKind::Spot => "Spot",
                MarketKind::LinearPerps => "Linear",
                MarketKind::InversePerps => "Inverse",
            }
        )
    }
}

impl FromStr for MarketKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("spot") {
            Ok(Self::Spot)
        } else if s.eq_ignore_ascii_case("linear") {
            Ok(Self::LinearPerps)
        } else if s.eq_ignore_ascii_case("inverse") {
            Ok(Self::InversePerps)
        } else {
            Err(format!("Invalid market kind: {}", s))
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum StreamKind {
    Kline {
        ticker_info: TickerInfo,
        timeframe: Timeframe,
    },
    Depth {
        ticker_info: TickerInfo,
        #[serde(default = "default_depth_aggr")]
        depth_aggr: StreamTicksize,
        push_freq: PushFrequency,
    },
    Trades {
        ticker_info: TickerInfo,
    },
}

impl StreamKind {
    pub fn ticker_info(&self) -> TickerInfo {
        match self {
            StreamKind::Kline { ticker_info, .. }
            | StreamKind::Depth { ticker_info, .. }
            | StreamKind::Trades { ticker_info, .. } => *ticker_info,
        }
    }

    pub fn as_depth_stream(&self) -> Option<(TickerInfo, StreamTicksize, PushFrequency)> {
        match self {
            StreamKind::Depth {
                ticker_info,
                depth_aggr,
                push_freq,
            } => Some((*ticker_info, *depth_aggr, *push_freq)),
            _ => None,
        }
    }

    pub fn as_trade_stream(&self) -> Option<TickerInfo> {
        match self {
            StreamKind::Trades { ticker_info } => Some(*ticker_info),
            _ => None,
        }
    }

    pub fn as_kline_stream(&self) -> Option<(TickerInfo, Timeframe)> {
        match self {
            StreamKind::Kline {
                ticker_info,
                timeframe,
            } => Some((*ticker_info, *timeframe)),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct UniqueStreams {
    streams: EnumMap<Exchange, Option<FxHashMap<TickerInfo, FxHashSet<StreamKind>>>>,
    specs: EnumMap<Exchange, Option<StreamSpecs>>,
}

impl UniqueStreams {
    pub fn from<'a>(streams: impl Iterator<Item = &'a StreamKind>) -> Self {
        let mut unique_streams = UniqueStreams::default();
        for stream in streams {
            unique_streams.add(*stream);
        }
        unique_streams
    }

    pub fn add(&mut self, stream: StreamKind) {
        let (exchange, ticker_info) = match stream {
            StreamKind::Kline { ticker_info, .. }
            | StreamKind::Depth { ticker_info, .. }
            | StreamKind::Trades { ticker_info, .. } => (ticker_info.exchange(), ticker_info),
        };

        self.streams[exchange]
            .get_or_insert_with(FxHashMap::default)
            .entry(ticker_info)
            .or_default()
            .insert(stream);

        self.update_specs_for_exchange(exchange);
    }

    pub fn extend<'a>(&mut self, streams: impl IntoIterator<Item = &'a StreamKind>) {
        for stream in streams {
            self.add(*stream);
        }
    }

    fn update_specs_for_exchange(&mut self, exchange: Exchange) {
        let depth_streams = self.depth_streams(Some(exchange));
        let trade_streams = self.trade_streams(Some(exchange));
        let kline_streams = self.kline_streams(Some(exchange));

        self.specs[exchange] = Some(StreamSpecs {
            depth: depth_streams,
            trade: trade_streams,
            kline: kline_streams,
        });
    }

    fn streams<T, F>(&self, exchange_filter: Option<Exchange>, stream_extractor: F) -> Vec<T>
    where
        F: Fn(Exchange, &StreamKind) -> Option<T>,
    {
        let f = &stream_extractor;

        let per_exchange = |exchange| {
            self.streams[exchange]
                .as_ref()
                .into_iter()
                .flat_map(|ticker_map| ticker_map.values().flatten())
                .filter_map(move |stream| f(exchange, stream))
        };

        match exchange_filter {
            Some(exchange) => per_exchange(exchange).collect(),
            None => Exchange::ALL.into_iter().flat_map(per_exchange).collect(),
        }
    }

    pub fn depth_streams(
        &self,
        exchange_filter: Option<Exchange>,
    ) -> Vec<(TickerInfo, StreamTicksize, PushFrequency)> {
        self.streams(exchange_filter, |_, stream| stream.as_depth_stream())
    }

    pub fn kline_streams(&self, exchange_filter: Option<Exchange>) -> Vec<(TickerInfo, Timeframe)> {
        self.streams(exchange_filter, |_, stream| stream.as_kline_stream())
    }

    pub fn trade_streams(&self, exchange_filter: Option<Exchange>) -> Vec<TickerInfo> {
        self.streams(exchange_filter, |_, stream| stream.as_trade_stream())
    }

    pub fn combined_used(&self) -> impl Iterator<Item = (Exchange, &StreamSpecs)> {
        self.specs
            .iter()
            .filter_map(|(exchange, specs)| specs.as_ref().map(|stream| (exchange, stream)))
    }

    pub fn combined(&self) -> &EnumMap<Exchange, Option<StreamSpecs>> {
        &self.specs
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum StreamTicksize {
    ServerSide(TickMultiplier),
    #[default]
    Client,
}

fn default_depth_aggr() -> StreamTicksize {
    StreamTicksize::Client
}

#[derive(Debug, Clone, Default)]
pub struct StreamSpecs {
    pub depth: Vec<(TickerInfo, StreamTicksize, PushFrequency)>,
    pub trade: Vec<TickerInfo>,
    pub kline: Vec<(TickerInfo, Timeframe)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum Venue {
    Binance,
    SSZ,
    SSH,
}

impl Venue {
    pub const ALL: [Venue; 3] = [Venue::Binance, Venue::SSZ, Venue::SSH];
}

impl std::fmt::Display for Venue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Venue::Binance => "Binance",
                Venue::SSZ => "SSZ",
                Venue::SSH => "SSH",
            }
        )
    }
}

impl FromStr for Venue {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("binance") {
            Ok(Self::Binance)
        } else if s.eq_ignore_ascii_case("ssz") {
            Ok(Self::SSZ)
        } else if s.eq_ignore_ascii_case("ssh") {
            Ok(Self::SSH)
        } else {
            Err(format!("Invalid venue: {}", s))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, Enum)]
pub enum Exchange {
    BinanceLinear,
    BinanceInverse,
    BinanceSpot,
    SSZ,
    SSH,
}

impl std::fmt::Display for Exchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Exchange::BinanceLinear => "Binance Linear",
                Exchange::BinanceInverse => "Binance Inverse",
                Exchange::BinanceSpot => "Binance Spot",
                Exchange::SSZ => "SSZ",
                Exchange::SSH => "SSH",
            }
        )
    }
}

impl FromStr for Exchange {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Binance Linear" | "BinanceLinear" => Ok(Exchange::BinanceLinear),
            "Binance Inverse" | "BinanceInverse" => Ok(Exchange::BinanceInverse),
            "Binance Spot" | "BinanceSpot" => Ok(Exchange::BinanceSpot),
            "SSZ" | "SSZ Spot" | "SSZSpot" => Ok(Exchange::SSZ),
            "SSH" | "SSH Spot" | "SSHSpot" => Ok(Exchange::SSH),
            _ => Err(format!("Invalid exchange: {}", s)),
        }
    }
}

impl Exchange {
    pub const ALL: [Exchange; 5] = [
        Exchange::BinanceLinear,
        Exchange::BinanceInverse,
        Exchange::BinanceSpot,
        Exchange::SSZ,
        Exchange::SSH,
    ];

    pub fn from_venue_and_market(venue: Venue, market: MarketKind) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|exchange| exchange.venue() == venue && exchange.market_type() == market)
    }

    pub fn market_type(&self) -> MarketKind {
        match self {
            Exchange::BinanceLinear => MarketKind::LinearPerps,
            Exchange::BinanceInverse => MarketKind::InversePerps,
            Exchange::BinanceSpot | Exchange::SSZ | Exchange::SSH => MarketKind::Spot,
        }
    }

    pub fn venue(&self) -> Venue {
        match self {
            Exchange::BinanceLinear | Exchange::BinanceInverse | Exchange::BinanceSpot => {
                Venue::Binance
            }
            Exchange::SSZ => Venue::SSZ,
            Exchange::SSH => Venue::SSH,
        }
    }

    pub fn is_depth_client_aggr(&self) -> bool {
        matches!(
            self,
            Exchange::BinanceLinear
                | Exchange::BinanceInverse
                | Exchange::BinanceSpot
                | Exchange::SSZ
                | Exchange::SSH
        )
    }

    pub fn is_custom_push_freq(&self) -> bool {
        false
    }

    pub fn allowed_push_freqs(&self) -> &[PushFrequency] {
        &[PushFrequency::ServerDefault]
    }

    pub fn supports_heatmap_timeframe(&self, tf: Timeframe) -> bool {
        match self.venue() {
            Venue::Binance => Timeframe::HEATMAP.contains(&tf),
            Venue::SSZ | Venue::SSH => matches!(tf, Timeframe::MS3000),
        }
    }

    pub fn supports_custom_minutes_timeframe(&self) -> bool {
        matches!(self.venue(), Venue::SSZ | Venue::SSH)
    }

    pub fn supports_kline_timeframe(&self, tf: Timeframe) -> bool {
        match self.venue() {
            Venue::Binance => Timeframe::KLINE.contains(&tf),
            Venue::SSZ | Venue::SSH => {
                matches!(tf, Timeframe::D1) || tf.to_milliseconds() >= 60_000
            }
        }
    }

    pub fn is_perps(&self) -> bool {
        matches!(self, Exchange::BinanceLinear | Exchange::BinanceInverse)
    }

    pub fn stream_ticksize(
        &self,
        multiplier: Option<TickMultiplier>,
        server_fallback: TickMultiplier,
    ) -> StreamTicksize {
        if self.is_depth_client_aggr() {
            StreamTicksize::Client
        } else {
            StreamTicksize::ServerSide(multiplier.unwrap_or(server_fallback))
        }
    }

    pub fn is_symbol_supported(&self, symbol: &str, log: bool) -> bool {
        let valid_symbol = symbol
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');

        if valid_symbol {
            return true;
        } else if log {
            log::warn!("Unsupported ticker: '{}': {:?}", self, symbol,);
        }
        false
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Connected(Exchange),
    Disconnected(Exchange, String),
    DepthReceived(StreamKind, u64, Arc<Depth>),
    TradesReceived(StreamKind, u64, Box<[Trade]>),
    KlineReceived(StreamKind, Kline),
}

#[derive(Debug, Clone, Hash)]
pub struct StreamConfig<I> {
    pub id: I,
    pub exchange: Exchange,
    pub tick_mltp: Option<TickMultiplier>,
    pub push_freq: PushFrequency,
}

impl<I> StreamConfig<I> {
    pub fn new(
        id: I,
        exchange: Exchange,
        tick_mltp: Option<TickMultiplier>,
        push_freq: PushFrequency,
    ) -> Self {
        Self {
            id,
            exchange,
            tick_mltp,
            push_freq,
        }
    }
}

/// Returns a map of tickers to their [`TickerInfo`].
/// If metadata for a ticker can't be fetched/parsed expectedly, it will still be included in the map as `None`.
pub async fn fetch_ticker_metadata(
    venue: Venue,
    markets: &[MarketKind],
) -> Result<HashMap<Ticker, Option<TickerInfo>>, AdapterError> {
    match venue {
        Venue::Binance => {
            let mut out = HashMap::default();
            for market in markets {
                out.extend(binance::fetch_ticker_metadata(*market).await?);
            }
            Ok(out)
        }
        Venue::SSZ | Venue::SSH => qmt::fetch_ticker_metadata(venue).await,
    }
}

pub async fn search_ticker_metadata(
    venue: Venue,
    query: &str,
    limit: usize,
) -> Result<HashMap<Ticker, Option<TickerInfo>>, AdapterError> {
    match venue {
        Venue::Binance => Err(AdapterError::InvalidRequest(
            "On-demand ticker search is only implemented for QMT venues".to_string(),
        )),
        Venue::SSZ | Venue::SSH => qmt::search_ticker_metadata(venue, query, limit).await,
    }
}

/// Returns a map of tickers to their [`TickerStats`].
pub async fn fetch_ticker_stats(
    venue: Venue,
    markets: &[MarketKind],
    contract_sizes: Option<HashMap<Ticker, f32>>,
) -> Result<HashMap<Ticker, TickerStats>, AdapterError> {
    match venue {
        Venue::Binance => {
            let mut out = HashMap::default();
            for market in markets {
                out.extend(binance::fetch_ticker_stats(*market, contract_sizes.as_ref()).await?);
            }
            Ok(out)
        }
        Venue::SSZ | Venue::SSH => qmt::fetch_ticker_stats(venue).await,
    }
}

pub async fn fetch_klines(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(u64, u64)>,
) -> Result<Vec<Kline>, AdapterError> {
    match ticker_info.ticker.exchange.venue() {
        Venue::Binance => binance::fetch_klines(ticker_info, timeframe, range).await,
        Venue::SSZ | Venue::SSH => qmt::fetch_klines(ticker_info, timeframe, range).await,
    }
}

pub async fn fetch_klines_and_trades(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(u64, u64)>,
) -> Result<(Vec<Kline>, Vec<Trade>), AdapterError> {
    match ticker_info.ticker.exchange.venue() {
        Venue::Binance => Err(AdapterError::InvalidRequest(
            "Combined historical kline/trade fetch is only implemented for QMT venues".to_string(),
        )),
        Venue::SSZ | Venue::SSH => {
            qmt::fetch_klines_and_trades(ticker_info, timeframe, range).await
        }
    }
}

pub async fn fetch_trades(
    ticker_info: TickerInfo,
    range: (u64, u64),
) -> Result<Vec<Trade>, AdapterError> {
    match ticker_info.ticker.exchange.venue() {
        Venue::Binance => Err(AdapterError::InvalidRequest(
            "Generic historical trade fetch is only implemented for QMT venues".to_string(),
        )),
        Venue::SSZ | Venue::SSH => qmt::fetch_trades(ticker_info, range).await,
    }
}

pub async fn fetch_open_interest(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(u64, u64)>,
) -> Result<Vec<OpenInterest>, AdapterError> {
    let exchange = ticker_info.ticker.exchange;

    match exchange {
        Exchange::BinanceLinear | Exchange::BinanceInverse => {
            binance::fetch_historical_oi(ticker_info, range, timeframe).await
        }
        _ => Err(AdapterError::InvalidRequest(format!(
            "Open interest data not available for {exchange}"
        ))),
    }
}
