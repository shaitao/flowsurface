use super::*;
use futures::SinkExt;
use tokio::sync::broadcast;

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

pub(super) fn build_depth_history_from_ticks(
    ticks: &[QmtTick],
    ticker_info: TickerInfo,
) -> Vec<(u64, crate::depth::Depth)> {
    let mut depth_cache = LocalDepthCache::default();
    let mut snapshots = Vec::new();

    for tick in ticks {
        let payload = tick_to_depth_payload(tick);
        if payload.bids.is_empty() && payload.asks.is_empty() {
            continue;
        }

        depth_cache.update(DepthUpdate::Snapshot(payload), ticker_info.min_ticksize);
        snapshots.push((tick.time, depth_cache.depth.as_ref().clone()));
    }

    snapshots
}

fn trade_flush_map(ticker_info: TickerInfo) -> FxHashMap<Ticker, (TickerInfo, ())> {
    FxHashMap::from_iter([(ticker_info.ticker, (ticker_info, ()))])
}

fn qmt_single_ticker_message(capability: &str) -> String {
    format!("QMT {capability} currently supports one live ticker at a time")
}

async fn recv_shared_tick_event(
    receiver: &mut broadcast::Receiver<SharedTickStreamEvent>,
    ticker_info: TickerInfo,
) -> Option<SharedTickStreamEvent> {
    loop {
        match receiver.recv().await {
            Ok(event) => return Some(event),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                log::warn!(
                    "QMT shared tick stream lagged for {}: skipped {} messages",
                    ticker_info.ticker,
                    skipped
                );
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
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
        let mut depth_cache = LocalDepthCache::default();
        let mut receiver = subscribe_shared_tick_stream(ticker_info).await;

        while let Some(event) = recv_shared_tick_event(&mut receiver, ticker_info).await {
            match event {
                SharedTickStreamEvent::Connected => {
                    let _ = output.send(Event::Connected(exchange)).await;
                }
                SharedTickStreamEvent::Disconnected(reason) => {
                    let _ = output.send(Event::Disconnected(exchange, reason)).await;
                }
                SharedTickStreamEvent::Tick(tick) => {
                    let payload = tick_to_depth_payload(&tick);
                    if payload.bids.is_empty() && payload.asks.is_empty() {
                        continue;
                    }
                    depth_cache.update(DepthUpdate::Snapshot(payload), ticker_info.min_ticksize);
                    let _ = output
                        .send(Event::DepthReceived(
                            stream_kind,
                            tick.time,
                            depth_cache.depth.clone(),
                        ))
                        .await;
                }
            }
        }

        let _ = output
            .send(Event::Disconnected(
                exchange,
                "QMT shared tick stream closed".to_string(),
            ))
            .await;
    })
}

pub fn connect_trade_stream(
    tickers: Vec<TickerInfo>,
    _market_type: super::super::MarketKind,
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

        let mut trade_state = SyntheticTradeState::default();
        let ticker_info_map = trade_flush_map(ticker_info);
        let mut trade_buffers = FxHashMap::from_iter([(ticker_info.ticker, Vec::<Trade>::new())]);
        let mut last_flush = tokio::time::Instant::now();
        let mut receiver = subscribe_shared_tick_stream(ticker_info).await;

        while let Some(event) = recv_shared_tick_event(&mut receiver, ticker_info).await {
            match event {
                SharedTickStreamEvent::Connected => {
                    trade_state = SyntheticTradeState::default();
                    last_flush = tokio::time::Instant::now();
                    let _ = output.send(Event::Connected(exchange)).await;
                }
                SharedTickStreamEvent::Disconnected(reason) => {
                    flush_trade_buffers(&mut output, &ticker_info_map, &mut trade_buffers).await;
                    let _ = output.send(Event::Disconnected(exchange, reason)).await;
                }
                SharedTickStreamEvent::Tick(tick) => {
                    let day = china_trading_day(tick.time);
                    let history_ready =
                        day.is_some_and(|day| current_day_history_ready(ticker_info.ticker, day));
                    let trades = trade_state.update(tick, ticker_info);

                    if history_ready && !trades.is_empty() {
                        trade_buffers
                            .entry(ticker_info.ticker)
                            .or_default()
                            .extend(trades);
                    }

                    if last_flush.elapsed() >= super::super::TRADE_BUCKET_INTERVAL {
                        flush_trade_buffers(&mut output, &ticker_info_map, &mut trade_buffers)
                            .await;
                        last_flush = tokio::time::Instant::now();
                    }
                }
            }
        }

        flush_trade_buffers(&mut output, &ticker_info_map, &mut trade_buffers).await;
        let _ = output
            .send(Event::Disconnected(
                exchange,
                "QMT shared tick stream closed".to_string(),
            ))
            .await;
    })
}

pub fn connect_kline_stream(
    streams: Vec<(TickerInfo, Timeframe)>,
    _market_type: super::super::MarketKind,
) -> impl Stream<Item = Event> {
    channel(100, move |mut output| async move {
        let Some((ticker_info, _)) = streams.first().copied() else {
            return;
        };
        let exchange = ticker_info.exchange();

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

        let mut receiver = subscribe_shared_tick_stream(ticker_info).await;

        while let Some(event) = recv_shared_tick_event(&mut receiver, ticker_info).await {
            match event {
                SharedTickStreamEvent::Connected => {
                    let _ = output.send(Event::Connected(exchange)).await;
                }
                SharedTickStreamEvent::Disconnected(reason) => {
                    let _ = output.send(Event::Disconnected(exchange, reason)).await;
                }
                SharedTickStreamEvent::Tick(tick) => {
                    let day = china_trading_day(tick.time);
                    let history_ready =
                        day.is_some_and(|day| current_day_history_ready(ticker_info.ticker, day));

                    if !history_ready {
                        continue;
                    }

                    for stream in &live_streams {
                        if let Some(kline) =
                            build_live_kline_snapshot(stream.ticker_info, stream.timeframe, &tick)
                        {
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
            }
        }

        let _ = output
            .send(Event::Disconnected(
                exchange,
                "QMT shared tick stream closed".to_string(),
            ))
            .await;
    })
}
