use crate::{
    OpenInterest, Price, PushFrequency,
    adapter::{StreamKind, StreamTicksize},
};

use super::{
    super::{
        Exchange, Kline, MarketKind, Ticker, TickerInfo, TickerStats, Timeframe, Trade,
    },
    AdapterError, Event,
};

use super::super::depth::{DeOrder, DepthPayload, DepthUpdate, LocalDepthCache};

use iced_futures::{
    futures::{SinkExt, Stream},
    stream,
};

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};


#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct QuoteBook {
    pub time: i64, // 13 位毫秒时间戳
    #[serde(rename = "lastPrice")]
    pub last_price: f32,
    pub open: f32,
    pub high: f32,
    pub low: f32,
    #[serde(rename = "lastClose")]
    pub last_close: f32,
    pub amount: f64,  // 成交额
    pub volume: u64,  // 成交量（手/股）
    pub pvolume: u64, // 累计成交量？
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

#[derive(Deserialize)]
struct QMTL1Data {
    pub time: u64,
    pub price: f32,
    pub volume: u64,
    pub is_sell: bool,
    pub update_id: u64,
    pub bids: Vec<DeOrder>,
    pub asks: Vec<DeOrder>,
}

// 顶层：以股票代码为 key 的映射
pub type QuotesByTicker = std::collections::HashMap<String, QuoteBook>;

pub fn connect_market_stream(
    ticker_info: TickerInfo,
    push_freq: PushFrequency,
) -> impl Stream<Item = Event> {
    stream::channel(100, async move |mut output| {
        let ticker = ticker_info.ticker;

        let (symbol_str, market_type) = ticker.to_full_symbol_and_type();
        let exchange = ticker.exchange;
        println!(
            "QMT connecting to market stream for {} {} {}",
            symbol_str, market_type, exchange
        );

        let conent = tokio::fs::read_to_string("YGDY-ticks-realtime.jsonl")
            .await
            .unwrap();

        let ticks = conent
            .lines()
            .map(|line| {
                let de_trade: HashMap<String, QuoteBook> = serde_json::from_str(line).unwrap();
                de_trade.get("300274.SZ").cloned().unwrap()
            })
            .collect::<Vec<QuoteBook>>();

        let mut ticks_iter = ticks
            .windows(2)
            .map(|w| {
                let prev = &w[0];
                let curr = &w[1];
                let is_sell = {
                    let ask1 = curr.ask_price[0];
                    let bid1 = curr.bid_price[0];

                    if curr.last_price >= ask1 {
                        false // 吃到卖一 => 买单
                    } else if curr.last_price <= bid1 {
                        true // 吃到买一 => 卖单
                    } else {
                        curr.last_price < prev.last_price // 价跌认为是卖
                    }
                };

                let data: QMTL1Data = QMTL1Data {
                    time: curr.time as u64,
                    update_id: curr.time as u64,
                    price: curr.last_price as f32,
                    volume: curr.volume - prev.volume,
                    is_sell,
                    bids: curr
                        .bid_price
                        .iter()
                        .zip(curr.bid_vol.iter())
                        .map(|(p, v)| DeOrder {
                            price: *p as f32,
                            qty: *v as f32,
                        })
                        .collect(),
                    asks: curr
                        .ask_price
                        .iter()
                        .zip(curr.ask_vol.iter())
                        .map(|(p, v)| DeOrder {
                            price: *p as f32,
                            qty: *v as f32,
                        })
                        .collect(),
                };

                data
            })
            .filter(|t| t.volume > 0);

        let mut trades_buffer: Vec<Trade> = vec![];
        let mut orderbook = LocalDepthCache::default();

        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            if let Some(tick) = ticks_iter.next() {
                let price = Price::from_f32(tick.price).round_to_min_tick(ticker_info.min_ticksize);
                // Trade
                let trade = Trade {
                    time: tick.time,
                    price,
                    qty: tick.volume as f32,
                    is_sell: tick.is_sell,
                };
                trades_buffer.push(trade);

                // Orderbook
                let depth_update = DepthUpdate::Snapshot(DepthPayload {
                    last_update_id: tick.update_id,
                    time: tick.time,
                    bids: tick.bids.clone(),
                    asks: tick.asks.clone(),
                });

                orderbook.update(depth_update, ticker_info.min_ticksize);

                let _ = output
                    .send(Event::DepthReceived(
                        StreamKind::DepthAndTrades {
                            ticker_info,
                            depth_aggr: StreamTicksize::Client,
                            push_freq,
                        },
                        tick.time,
                        orderbook.depth.clone(),
                        std::mem::take(&mut trades_buffer).into_boxed_slice(),
                    ))
                    .await;
            }
        }
    })
}

pub fn connect_kline_stream(
    streams: Vec<(TickerInfo, Timeframe)>,
    _market_type: MarketKind,
) -> impl Stream<Item = Event> {
    println!(
        "QMT connecting to kline stream for {} tickers",
        streams.len()
    );
    stream::channel(100, async move |_output| {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    })
}

pub async fn fetch_ticksize()
-> Result<std::collections::HashMap<Ticker, Option<TickerInfo>>, AdapterError> {
    println!("QMT fetching ticksize");
    let mut map = HashMap::new();
    let ticker = Ticker::new("300274.SZ", Exchange::SSZ);
    let info = TickerInfo::new(ticker, 0.01, 1.0, None);
    map.insert(ticker, Some(info));
    Ok(map)
}

pub async fn fetch_ticker_prices(
    _market_type: MarketKind,
) -> Result<std::collections::HashMap<Ticker, TickerStats>, AdapterError> {
    println!("QMT fetching ticker prices");
    let mut map = HashMap::new();
    let ticker = Ticker::new("300274.SZ", Exchange::SSZ);
    let state = TickerStats {
        mark_price: 170.0,
        daily_price_chg: 0.0,
        daily_volume: 1.0,
    };
    map.insert(ticker, state);
    Ok(map)
}

pub async fn fetch_klines(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(u64, u64)>,
) -> Result<Vec<Kline>, AdapterError> {
    println!(
        "QMT fetching klines for {} {} {:?}",
        ticker_info.ticker, timeframe, range
    );
    Ok(vec![])
}

pub async fn fetch_historical_oi(
    _ticker: Ticker,
    _range: Option<(u64, u64)>,
    _period: Timeframe,
) -> Result<Vec<OpenInterest>, AdapterError> {
    Ok(vec![])
}
