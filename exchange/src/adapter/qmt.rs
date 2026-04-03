use crate::{
    OpenInterest, Price, PushFrequency, Volume,
    adapter::{StreamKind, StreamTicksize, Venue},
};

use super::{
    super::{
        Exchange, Kline, MarketKind, Ticker, TickerInfo, TickerStats, Timeframe, Trade,
        connect::channel,
        depth::{DeOrder, DepthPayload, DepthUpdate, LocalDepthCache},
        unit::Qty,
    },
    AdapterError, Event,
};

use futures::{SinkExt, Stream};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};

const MOCK_TICKS_FILE: &str = "YGDY-ticks-realtime.jsonl";
const RETRY_DELAY: Duration = Duration::from_secs(5);
const STREAM_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct QuoteBook {
    pub time: i64,
    #[serde(rename = "lastPrice")]
    pub last_price: f32,
    pub open: f32,
    pub high: f32,
    pub low: f32,
    #[serde(rename = "lastClose")]
    pub last_close: f32,
    pub amount: f64,
    pub volume: u64,
    pub pvolume: u64,
    #[serde(rename = "stockStatus")]
    pub stock_status: i32,
    #[serde(rename = "openInt")]
    pub open_int: i32,
    #[serde(rename = "transactionNum")]
    pub transaction_num: i32,
    #[serde(rename = "lastSettlementPrice")]
    pub last_settlement_price: f64,
    #[serde(rename = "settlementPrice")]
    pub settlement_price: f64,
    pub pe: f64,
    #[serde(rename = "askPrice")]
    pub ask_price: Vec<f32>,
    #[serde(rename = "bidPrice")]
    pub bid_price: Vec<f32>,
    #[serde(rename = "askVol")]
    pub ask_vol: Vec<u64>,
    #[serde(rename = "bidVol")]
    pub bid_vol: Vec<u64>,
    #[serde(rename = "volRatio")]
    pub vol_ratio: f32,
    #[serde(rename = "speed1Min")]
    pub speed1_min: f32,
    #[serde(rename = "speed5Min")]
    pub speed5_min: f32,
}

#[derive(Clone)]
struct QmtL1Data {
    time: u64,
    price: f32,
    volume: u64,
    is_sell: bool,
    update_id: u64,
    bids: Vec<DeOrder>,
    asks: Vec<DeOrder>,
}

type QuotesByTicker = HashMap<String, QuoteBook>;

fn sample_info(venue: Venue) -> TickerInfo {
    let (exchange, symbol) = match venue {
        Venue::SSZ => (Exchange::SSZ, "300274.SZ"),
        Venue::SSH => (Exchange::SSH, "600519.SH"),
        Venue::Binance => unreachable!("qmt only supports stock venues"),
    };

    let ticker = Ticker::new(symbol, exchange);
    TickerInfo::new(ticker, 0.01, 1.0, None)
}

fn sample_last_price(venue: Venue) -> f32 {
    match venue {
        Venue::SSZ => 170.0,
        Venue::SSH => 180.0,
        Venue::Binance => unreachable!("qmt only supports stock venues"),
    }
}

fn build_tick(prev: &QuoteBook, curr: &QuoteBook) -> Option<QmtL1Data> {
    if curr.volume <= prev.volume {
        return None;
    }

    let ask1 = curr.ask_price.first().copied().unwrap_or(curr.last_price);
    let bid1 = curr.bid_price.first().copied().unwrap_or(curr.last_price);
    let is_sell = if curr.last_price >= ask1 {
        false
    } else if curr.last_price <= bid1 {
        true
    } else {
        curr.last_price < prev.last_price
    };

    Some(QmtL1Data {
        time: curr.time as u64,
        update_id: curr.time as u64,
        price: curr.last_price,
        volume: curr.volume - prev.volume,
        is_sell,
        bids: curr
            .bid_price
            .iter()
            .zip(curr.bid_vol.iter())
            .map(|(p, v)| DeOrder {
                price: *p,
                qty: *v as f32,
            })
            .collect(),
        asks: curr
            .ask_price
            .iter()
            .zip(curr.ask_vol.iter())
            .map(|(p, v)| DeOrder {
                price: *p,
                qty: *v as f32,
            })
            .collect(),
    })
}

async fn load_ticks(symbol: &str) -> Result<Vec<QmtL1Data>, AdapterError> {
    let content = tokio::fs::read_to_string(MOCK_TICKS_FILE)
        .await
        .map_err(|e| AdapterError::InvalidRequest(format!("Failed to read {MOCK_TICKS_FILE}: {e}")))?;

    let mut prev: Option<QuoteBook> = None;
    let mut ticks = Vec::new();

    for line in content.lines() {
        let quotes: QuotesByTicker =
            serde_json::from_str(line).map_err(|e| AdapterError::ParseError(e.to_string()))?;

        let current = quotes
            .get(symbol)
            .cloned()
            .or_else(|| quotes.values().next().cloned());

        let Some(current) = current else {
            continue;
        };

        if let Some(previous) = prev.replace(current.clone())
            && let Some(tick) = build_tick(&previous, &current)
        {
            ticks.push(tick);
        }
    }

    Ok(ticks)
}

pub fn connect_depth_stream(
    ticker_info: TickerInfo,
    push_freq: PushFrequency,
) -> impl Stream<Item = Event> {
    channel(100, move |mut output| async move {
        let exchange = ticker_info.exchange();
        let symbol = ticker_info.ticker.to_string();

        loop {
            match load_ticks(&symbol).await {
                Ok(ticks) => {
                    let _ = output.send(Event::Connected(exchange)).await;
                    let mut orderbook = LocalDepthCache::default();

                    for tick in ticks {
                        orderbook.update(
                            DepthUpdate::Snapshot(DepthPayload {
                                last_update_id: tick.update_id,
                                time: tick.time,
                                bids: tick.bids,
                                asks: tick.asks,
                            }),
                            ticker_info.min_ticksize,
                        );

                        let _ = output
                            .send(Event::DepthReceived(
                                StreamKind::Depth {
                                    ticker_info,
                                    depth_aggr: StreamTicksize::Client,
                                    push_freq,
                                },
                                tick.time,
                                Arc::clone(&orderbook.depth),
                            ))
                            .await;

                        tokio::time::sleep(STREAM_INTERVAL).await;
                    }
                }
                Err(err) => {
                    let _ = output
                        .send(Event::Disconnected(exchange, err.to_string()))
                        .await;
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    })
}

pub fn connect_trade_stream(
    tickers: Vec<TickerInfo>,
    _market_type: MarketKind,
) -> impl Stream<Item = Event> {
    channel(100, move |mut output| async move {
        let Some(exchange) = tickers.first().map(TickerInfo::exchange) else {
            return;
        };

        let _ = output.send(Event::Connected(exchange)).await;

        loop {
            for ticker_info in &tickers {
                let symbol = ticker_info.ticker.to_string();

                match load_ticks(&symbol).await {
                    Ok(ticks) => {
                        for tick in ticks {
                            let price =
                                Price::from_f32(tick.price).round_to_min_tick(ticker_info.min_ticksize);
                            let trade = Trade {
                                time: tick.time,
                                price,
                                qty: Qty::from_f32(tick.volume as f32),
                                is_sell: tick.is_sell,
                            };

                            let _ = output
                                .send(Event::TradesReceived(
                                    StreamKind::Trades {
                                        ticker_info: *ticker_info,
                                    },
                                    tick.time,
                                    vec![trade].into_boxed_slice(),
                                ))
                                .await;

                            tokio::time::sleep(STREAM_INTERVAL).await;
                        }
                    }
                    Err(err) => {
                        let _ = output
                            .send(Event::Disconnected(exchange, err.to_string()))
                            .await;
                    }
                }
            }

            tokio::time::sleep(RETRY_DELAY).await;
        }
    })
}

pub fn connect_kline_stream(
    streams: Vec<(TickerInfo, Timeframe)>,
    _market_type: MarketKind,
) -> impl Stream<Item = Event> {
    channel(100, move |mut output| async move {
        let Some(exchange) = streams.first().map(|(ticker_info, _)| ticker_info.exchange()) else {
            return;
        };

        let _ = output.send(Event::Connected(exchange)).await;

        loop {
            for (ticker_info, timeframe) in &streams {
                let symbol = ticker_info.ticker.to_string();

                match load_ticks(&symbol).await {
                    Ok(ticks) => {
                        for tick in ticks {
                            let kline = Kline::new(
                                tick.time,
                                tick.price,
                                tick.price,
                                tick.price,
                                tick.price,
                                Volume::TotalOnly(Qty::from_f32(tick.volume as f32)),
                                ticker_info.min_ticksize,
                            );

                            let _ = output
                                .send(Event::KlineReceived(
                                    StreamKind::Kline {
                                        ticker_info: *ticker_info,
                                        timeframe: *timeframe,
                                    },
                                    kline,
                                ))
                                .await;

                            tokio::time::sleep(STREAM_INTERVAL).await;
                        }
                    }
                    Err(err) => {
                        let _ = output
                            .send(Event::Disconnected(exchange, err.to_string()))
                            .await;
                    }
                }
            }

            tokio::time::sleep(RETRY_DELAY).await;
        }
    })
}

pub async fn fetch_ticker_metadata(
    venue: Venue,
) -> Result<HashMap<Ticker, Option<TickerInfo>>, AdapterError> {
    let info = sample_info(venue);
    let mut map = HashMap::new();
    map.insert(info.ticker, Some(info));
    Ok(map)
}

pub async fn fetch_ticker_stats(venue: Venue) -> Result<HashMap<Ticker, TickerStats>, AdapterError> {
    let info = sample_info(venue);
    let mut map = HashMap::new();
    map.insert(
        info.ticker,
        TickerStats {
            mark_price: Price::from_f32(sample_last_price(venue)),
            daily_price_chg: 0.0,
            daily_volume: Qty::from_f32(1.0),
        },
    );
    Ok(map)
}

pub async fn fetch_klines(
    ticker_info: TickerInfo,
    _timeframe: Timeframe,
    range: Option<(u64, u64)>,
) -> Result<Vec<Kline>, AdapterError> {
    let symbol = ticker_info.ticker.to_string();

    let ticks = match load_ticks(&symbol).await {
        Ok(ticks) => ticks,
        Err(err) => {
            log::warn!("QMT kline fallback for {symbol}: {err}");
            return Ok(vec![]);
        }
    };

    let klines = ticks
        .into_iter()
        .filter(|tick| {
            range
                .map(|(from, to)| tick.time >= from && tick.time <= to)
                .unwrap_or(true)
        })
        .map(|tick| {
            Kline::new(
                tick.time,
                tick.price,
                tick.price,
                tick.price,
                tick.price,
                Volume::TotalOnly(Qty::from_f32(tick.volume as f32)),
                ticker_info.min_ticksize,
            )
        })
        .collect();

    Ok(klines)
}

pub async fn fetch_historical_oi(
    _ticker_info: TickerInfo,
    _range: Option<(u64, u64)>,
    _period: Timeframe,
) -> Result<Vec<OpenInterest>, AdapterError> {
    Ok(vec![])
}
