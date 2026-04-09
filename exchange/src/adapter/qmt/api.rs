use super::*;

const QMT_CURRENT_DAY_TICK_FETCH_TTL: Duration = Duration::from_secs(2);

pub async fn fetch_ticker_metadata(
    _venue: Venue,
) -> Result<HashMap<Ticker, Option<TickerInfo>>, AdapterError> {
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
    Ok(
        fetch_tick_derived_history(ticker_info, timeframe, range, false)
            .await?
            .0,
    )
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
    for day in qmt_trading_days_between(venue, start_day, end_day)
        .into_iter()
        .rev()
    {
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

    log::info!(
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

pub async fn fetch_heatmap_history(
    ticker_info: TickerInfo,
    synthetic_book_levels: Option<u16>,
) -> Result<(Vec<Trade>, Vec<(u64, crate::depth::Depth)>), AdapterError> {
    let Some(day) = current_china_day() else {
        return Ok((Vec::new(), Vec::new()));
    };

    let Some(range) = qmt_current_day_history_bounds(day) else {
        return Ok((Vec::new(), Vec::new()));
    };

    let fetch_started_at = Instant::now();
    let ticks = fetch_ticks(ticker_info, range).await?;
    let fetch_elapsed = fetch_started_at.elapsed();
    let total_ticks = ticks.len();
    let venue = ticker_info.exchange().venue();

    let replay_ticks = ticks
        .into_iter()
        .filter(|tick| {
            qmt_tick_has_traded_volume(tick)
                && qmt_tick_has_top_of_book(tick)
                && qmt_heatmap_tick_in_session(venue, tick.time)
        })
        .collect::<Vec<_>>();

    let derive_started_at = Instant::now();
    let trades = synthesize_trades_from_ticks(&replay_ticks, ticker_info);
    let depths = build_depth_history_from_ticks(&replay_ticks, ticker_info, synthetic_book_levels);
    let derive_elapsed = derive_started_at.elapsed();

    log::info!(
        "QMT heatmap history {} ticks={} replay_ticks={} trades={} depths={} fetch_elapsed={:?} derive_elapsed={:?}",
        ticker_info.ticker,
        total_ticks,
        replay_ticks.len(),
        trades.len(),
        depths.len(),
        fetch_elapsed,
        derive_elapsed,
    );

    Ok((trades, depths))
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

async fn fetch_tick_day(
    ticker_info: TickerInfo,
    day: NaiveDate,
) -> Result<Vec<QmtTick>, AdapterError> {
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
    if let Some(ticks) = current_day_history_snapshot_if_fresh(
        ticker_info.ticker,
        day,
        QMT_CURRENT_DAY_TICK_FETCH_TTL,
    ) {
        return Ok(ticks);
    }

    if let Some(error) = recent_tick_fetch_failure(ticker_info.ticker, day) {
        return Err(AdapterError::InvalidRequest(format!(
            "QMT current-day tick fetch cooling down for {} on {} after previous failure: {}",
            ticker_info.ticker, day, error
        )));
    }

    let _fetch_guard = acquire_current_day_fetch_lock(ticker_info.ticker, day).await;

    if let Some(ticks) = current_day_history_snapshot_if_fresh(
        ticker_info.ticker,
        day,
        QMT_CURRENT_DAY_TICK_FETCH_TTL,
    ) {
        return Ok(ticks);
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
        ticker_info,
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
    Ok(sanitize_qmt_ticks(parsed.items))
}

pub async fn fetch_order_panel_snapshot(
    ticker_info: TickerInfo,
) -> Result<OrderPanelSnapshot, AdapterError> {
    let url = qmt_bridge_http_url(
        "/api/v1/order/panel",
        &[("symbol", ticker_info.ticker.to_string())],
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

    serde_json::from_str(&text).map_err(|e| AdapterError::ParseError(e.to_string()))
}

pub async fn submit_order(
    ticker_info: TickerInfo,
    request: OrderSubmitRequest,
) -> Result<OrderSubmitResponse, AdapterError> {
    let url = qmt_bridge_http_url("/api/v1/order/place", &[])?;
    let symbol = ticker_info.ticker.to_string();
    let body = BridgeOrderSubmitRequest {
        symbol: &symbol,
        side: request.side,
        order_type: request.order_type,
        price: request.price,
        quantity: request.quantity,
    };

    let response = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(AdapterError::from)?;
    let status = response.status();
    let text = response.text().await.map_err(AdapterError::from)?;

    if !status.is_success() {
        return Err(AdapterError::http_status_failed(
            status,
            format!("POST {url} failed: {text}"),
        ));
    }

    serde_json::from_str(&text).map_err(|e| AdapterError::ParseError(e.to_string()))
}

pub async fn cancel_order(
    ticker_info: TickerInfo,
    request: OrderCancelRequest,
) -> Result<OrderCancelResponse, AdapterError> {
    let url = qmt_bridge_http_url("/api/v1/order/cancel", &[])?;
    let symbol = ticker_info.ticker.to_string();
    let body = BridgeOrderCancelRequest {
        symbol: &symbol,
        order_id: &request.order_id,
    };

    let response = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(AdapterError::from)?;
    let status = response.status();
    let text = response.text().await.map_err(AdapterError::from)?;

    if !status.is_success() {
        return Err(AdapterError::http_status_failed(
            status,
            format!("POST {url} failed: {text}"),
        ));
    }

    serde_json::from_str(&text).map_err(|e| AdapterError::ParseError(e.to_string()))
}

pub async fn fetch_historical_oi(
    _ticker_info: TickerInfo,
    _range: Option<(u64, u64)>,
    _period: Timeframe,
) -> Result<Vec<OpenInterest>, AdapterError> {
    Ok(vec![])
}
