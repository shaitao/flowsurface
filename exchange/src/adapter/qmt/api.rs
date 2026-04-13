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
    let parsed: BridgeItemsResponse<BridgeSearchItem> = qmt_get_bridge(&url).await?;
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
        fetch_history_with_kline_fallback(ticker_info, timeframe, range, false)
            .await?
            .0,
    )
}

pub async fn fetch_klines_and_trades(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(u64, u64)>,
) -> Result<(Vec<Kline>, Vec<Trade>), AdapterError> {
    fetch_history_with_kline_fallback(ticker_info, timeframe, range, true).await
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
    let calendar_started_at = Instant::now();
    if let Err(error) = ensure_trading_calendar(venue, calendar_seed_start, requested_end).await {
        log::warn!(
            "QMT trading calendar seed fetch failed for {}: {error}",
            ticker_info.ticker
        );
    }
    let calendar_elapsed = calendar_started_at.elapsed();

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
        "QMT derived history {} {} latest_day_only={} requested={:?} effective=({}..{}) seed_start={} ticks={} trades={} klines={} calendar_elapsed={:?} fetch_elapsed={:?} derive_elapsed={:?} total_elapsed={:?}",
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
        calendar_elapsed,
        fetch_elapsed,
        derive_elapsed,
        total_started_at.elapsed()
    );

    Ok((klines, filtered_trades))
}

async fn fetch_history_with_kline_fallback(
    ticker_info: TickerInfo,
    timeframe: Timeframe,
    range: Option<(u64, u64)>,
    latest_day_only: bool,
) -> Result<(Vec<Kline>, Vec<Trade>), AdapterError> {
    match fetch_tick_derived_history(ticker_info, timeframe, range, latest_day_only).await {
        Ok((klines, trades)) if !klines.is_empty() || !trades.is_empty() => Ok((klines, trades)),
        Ok((klines, trades)) => {
            log::warn!(
                "QMT tick-derived history empty for {} {} latest_day_only={} range={:?}; falling back to 1m klines",
                ticker_info.ticker,
                timeframe,
                latest_day_only,
                range
            );
            match fetch_history_from_1m_klines(ticker_info, timeframe, range, latest_day_only).await
            {
                Ok(fallback) => {
                    if fallback.0.is_empty() {
                        Ok((klines, trades))
                    } else {
                        Ok(fallback)
                    }
                }
                Err(fallback_error) => {
                    log::error!(
                        "QMT 1m-kline fallback failed for {} {} latest_day_only={} range={:?}: {}; returning empty tick-derived history",
                        ticker_info.ticker,
                        timeframe,
                        latest_day_only,
                        range,
                        fallback_error
                    );
                    Ok((klines, trades))
                }
            }
        }
        Err(error) => {
            log::warn!(
                "QMT tick-derived history failed for {} {} latest_day_only={} range={:?}; falling back to 1m klines: {}",
                ticker_info.ticker,
                timeframe,
                latest_day_only,
                range,
                error
            );
            let fallback =
                fetch_history_from_1m_klines(ticker_info, timeframe, range, latest_day_only)
                    .await?;
            if fallback.0.is_empty() {
                Err(error)
            } else {
                Ok(fallback)
            }
        }
    }
}

fn qmt_kline_fallback_source_start(venue: Venue, start_ms: u64, timeframe: Timeframe) -> u64 {
    qmt_bucket_start(venue, start_ms, timeframe)
        .or_else(|| qmt_kline_seed_start(venue, start_ms))
        .unwrap_or(start_ms)
}

async fn fetch_history_from_1m_klines(
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
                "unsupported QMT timeframe for fallback klines: {timeframe}"
            ))
        })?;

    let venue = ticker_info.exchange().venue();
    let (start, end) = if latest_day_only {
        qmt_latest_history_chunk_range(venue, requested_start, requested_end)
            .unwrap_or((requested_start, requested_end))
    } else {
        (requested_start, requested_end)
    };

    let source_start = qmt_kline_fallback_source_start(venue, start, timeframe);
    let fetch_started_at = Instant::now();
    let source_bars = fetch_kline_chunk(ticker_info, "1m", (source_start, end)).await?;
    let fetch_elapsed = fetch_started_at.elapsed();

    let derive_started_at = Instant::now();
    let all_trades = synthesize_trades_from_1m_klines(&source_bars, ticker_info);
    let klines =
        aggregate_source_klines_to_klines(&source_bars, ticker_info, timeframe, start, end)?;
    let trades = all_trades
        .into_iter()
        .filter(|trade| start <= trade.time && trade.time <= end)
        .collect::<Vec<_>>();
    let derive_elapsed = derive_started_at.elapsed();

    log::info!(
        "QMT 1m-kline fallback {} {} latest_day_only={} requested={:?} effective=({}..{}) source_start={} source_bars={} klines={} trades={} fetch_elapsed={:?} derive_elapsed={:?} total_elapsed={:?}",
        ticker_info.ticker,
        timeframe,
        latest_day_only,
        range,
        start,
        end,
        source_start,
        source_bars.len(),
        klines.len(),
        trades.len(),
        fetch_elapsed,
        derive_elapsed,
        total_started_at.elapsed()
    );

    Ok((klines, trades))
}

pub async fn fetch_trades(
    ticker_info: TickerInfo,
    range: (u64, u64),
) -> Result<Vec<Trade>, AdapterError> {
    let (start, end) = range;
    match fetch_ticks(ticker_info, (start, end)).await {
        Ok(ticks) => {
            let trades = synthesize_trades_from_ticks(&ticks, ticker_info)
                .into_iter()
                .filter(|trade| start <= trade.time && trade.time <= end)
                .collect::<Vec<_>>();
            if !trades.is_empty() {
                return Ok(trades);
            }

            log::warn!(
                "QMT fetch_trades {} range=({}..{}) had no tick-derived trades; falling back to 1m klines",
                ticker_info.ticker,
                start,
                end
            );
        }
        Err(error) => {
            log::warn!(
                "QMT fetch_trades {} range=({}..{}) tick fetch failed; falling back to 1m klines: {}",
                ticker_info.ticker,
                start,
                end,
                error
            );
        }
    }

    let source_start =
        qmt_kline_fallback_source_start(ticker_info.exchange().venue(), start, Timeframe::M1);
    let source_bars = fetch_kline_chunk(ticker_info, "1m", (source_start, end)).await?;
    Ok(synthesize_trades_from_1m_klines(&source_bars, ticker_info)
        .into_iter()
        .filter(|trade| start <= trade.time && trade.time <= end)
        .collect())
}

pub async fn fetch_heatmap_history(
    ticker_info: TickerInfo,
    synthetic_book_levels: Option<u16>,
    range: Option<(u64, u64)>,
) -> Result<(Vec<Trade>, Vec<(u64, crate::depth::Depth)>), AdapterError> {
    let range = if let Some(range) = range {
        range
    } else {
        let venue = ticker_info.exchange().venue();
        let Some(day) = current_china_day() else {
            return Ok((Vec::new(), Vec::new()));
        };

        let Some((requested_start, requested_end)) = qmt_current_day_history_bounds(day) else {
            return Ok((Vec::new(), Vec::new()));
        };

        if let Err(error) = ensure_trading_calendar(
            venue,
            requested_start.saturating_sub(QMT_KLINE_SEED_CALENDAR_LOOKBACK_MS),
            requested_end,
        )
        .await
        {
            log::warn!(
                "QMT trading calendar seed fetch failed for {} heatmap default range: {error}",
                ticker_info.ticker
            );
        }

        let Some(range) = qmt_default_heatmap_history_bounds(venue, day) else {
            return Ok((Vec::new(), Vec::new()));
        };
        range
    };

    let (requested_start, requested_end) = range;

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
                && requested_start <= tick.time
                && tick.time <= requested_end
        })
        .collect::<Vec<_>>();

    let derive_started_at = Instant::now();
    let trades = synthesize_trades_from_ticks(&replay_ticks, ticker_info);
    let depths = build_depth_history_from_ticks(&replay_ticks, ticker_info, synthetic_book_levels);
    let derive_elapsed = derive_started_at.elapsed();

    log::info!(
        "QMT heatmap history {} range=({}..{}) ticks={} replay_ticks={} trades={} depths={} fetch_elapsed={:?} derive_elapsed={:?}",
        ticker_info.ticker,
        requested_start,
        requested_end,
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
    let total_started_at = Instant::now();
    let (start, end) = range;
    let venue = ticker_info.exchange().venue();
    let calendar_started_at = Instant::now();
    if let Err(error) = ensure_trading_calendar(venue, start, end).await {
        log::warn!(
            "QMT trading calendar fetch failed for {}: {error}",
            ticker_info.ticker
        );
    }
    let calendar_elapsed = calendar_started_at.elapsed();

    let Some((start_day, end_day)) = trading_day_range_from_timestamps(start, end) else {
        return Ok(Vec::new());
    };
    let trading_days = qmt_trading_days_between(venue, start_day, end_day);
    let trading_day_count = trading_days.len();

    if trading_days.is_empty() {
        return Ok(Vec::new());
    }

    if trading_day_count > 1 {
        log::info!(
            "QMT historical tick fetch for {} spans {} trading days",
            ticker_info.ticker,
            trading_day_count
        );
    }

    let mut ticks = Vec::new();
    for trading_day in trading_days {
        let day_started_at = Instant::now();
        let mut chunk = if current_china_day() == Some(trading_day) {
            fetch_current_day_ticks(ticker_info, trading_day).await?
        } else {
            fetch_tick_day(ticker_info, trading_day).await?
        };
        log::info!(
            "QMT fetch_ticks day {} {} rows={} elapsed={:?}",
            ticker_info.ticker,
            trading_day,
            chunk.len(),
            day_started_at.elapsed()
        );
        ticks.append(&mut chunk);
    }

    log::info!(
        "QMT fetch_ticks total {} range=({}..{}) trading_days={} rows={} calendar_elapsed={:?} total_elapsed={:?}",
        ticker_info.ticker,
        start,
        end,
        trading_day_count,
        ticks.len(),
        calendar_elapsed,
        total_started_at.elapsed()
    );

    Ok(ticks)
}

async fn fetch_tick_day(
    ticker_info: TickerInfo,
    day: NaiveDate,
) -> Result<Vec<QmtTick>, AdapterError> {
    let started_at = Instant::now();
    if let Some(error) = recent_tick_fetch_failure(ticker_info.ticker, day) {
        return Err(AdapterError::InvalidRequest(format!(
            "QMT historical tick fetch cooling down for {} on {} after previous failure: {}",
            ticker_info.ticker, day, error
        )));
    }

    if let Some(cached_ticks) = get_cached_tick_day(ticker_info.ticker, day) {
        log::info!(
            "QMT fetch_tick_day cache hit {} {} rows={} elapsed={:?}",
            ticker_info.ticker,
            day,
            cached_ticks.len(),
            started_at.elapsed()
        );
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
    log::info!(
        "QMT fetch_tick_day remote {} {} rows={} elapsed={:?}",
        ticker_info.ticker,
        day,
        ticks.len(),
        started_at.elapsed()
    );
    Ok(ticks)
}

async fn fetch_current_day_ticks(
    ticker_info: TickerInfo,
    day: NaiveDate,
) -> Result<Vec<QmtTick>, AdapterError> {
    let started_at = Instant::now();
    if let Some(ticks) = current_day_history_snapshot_if_fresh(
        ticker_info.ticker,
        day,
        QMT_CURRENT_DAY_TICK_FETCH_TTL,
    ) {
        log::info!(
            "QMT current-day tick snapshot hit {} {} rows={} elapsed={:?}",
            ticker_info.ticker,
            day,
            ticks.len(),
            started_at.elapsed()
        );
        return Ok(ticks);
    }

    if let Some(error) = recent_tick_fetch_failure(ticker_info.ticker, day) {
        return Err(AdapterError::InvalidRequest(format!(
            "QMT current-day tick fetch cooling down for {} on {} after previous failure: {}",
            ticker_info.ticker, day, error
        )));
    }

    let lock_started_at = Instant::now();
    let _fetch_guard = acquire_current_day_fetch_lock(ticker_info.ticker, day).await;
    let lock_elapsed = lock_started_at.elapsed();

    if let Some(ticks) = current_day_history_snapshot_if_fresh(
        ticker_info.ticker,
        day,
        QMT_CURRENT_DAY_TICK_FETCH_TTL,
    ) {
        log::info!(
            "QMT current-day tick snapshot hit after lock {} {} rows={} lock_elapsed={:?} total_elapsed={:?}",
            ticker_info.ticker,
            day,
            ticks.len(),
            lock_elapsed,
            started_at.elapsed()
        );
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
    let merged = merge_current_day_history_and_live(ticker_info, day, history_ticks);
    log::info!(
        "QMT current-day tick remote {} {} rows={} lock_elapsed={:?} total_elapsed={:?}",
        ticker_info.ticker,
        day,
        merged.len(),
        lock_elapsed,
        started_at.elapsed()
    );
    Ok(merged)
}

async fn fetch_tick_chunk(
    ticker_info: TickerInfo,
    range: (u64, u64),
) -> Result<Vec<QmtTick>, AdapterError> {
    let total_started_at = Instant::now();
    let (start, end) = range;
    let url = qmt_bridge_http_url(
        "/api/v1/ticks",
        &[
            ("symbol", ticker_info.ticker.to_string()),
            ("start", start.to_string()),
            ("end", end.to_string()),
        ],
    )?;

    let request_started_at = Instant::now();
    let response = qmt_bridge_http_client()
        .get(&url)
        .header(reqwest::header::ACCEPT, QMT_BRIDGE_MSGPACK_CONTENT_TYPE)
        .send()
        .await
        .map_err(AdapterError::from)?;
    let request_elapsed = request_started_at.elapsed();
    let status = response.status();
    let is_msgpack = qmt_response_is_msgpack(&response);
    let body_started_at = Instant::now();
    let bytes = response.bytes().await.map_err(AdapterError::from)?;
    let body_elapsed = body_started_at.elapsed();

    if !status.is_success() {
        return Err(AdapterError::http_status_failed(
            status,
            format!(
                "GET {url} failed: {}",
                qmt_response_body_to_error_detail(&bytes, is_msgpack)
            ),
        ));
    }

    if !is_msgpack {
        return Err(AdapterError::ParseError(format!(
            "QMT bridge returned unexpected content type for GET {url}"
        )));
    }

    let parse_started_at = Instant::now();
    let parsed: BridgeItemsResponse<QmtTick> = qmt_decode_msgpack(&bytes)?;
    let parse_elapsed = parse_started_at.elapsed();
    let sanitized = sanitize_qmt_ticks(parsed.items);
    log::info!(
        "QMT fetch_tick_chunk {} range=({}..{}) status={} rows={} request_elapsed={:?} body_elapsed={:?} parse_elapsed={:?} total_elapsed={:?}",
        ticker_info.ticker,
        start,
        end,
        status,
        sanitized.len(),
        request_elapsed,
        body_elapsed,
        parse_elapsed,
        total_started_at.elapsed()
    );
    Ok(sanitized)
}

async fn fetch_kline_chunk(
    ticker_info: TickerInfo,
    period: &str,
    range: (u64, u64),
) -> Result<Vec<QmtKlineBar>, AdapterError> {
    let total_started_at = Instant::now();
    let (start, end) = range;
    let url = qmt_bridge_http_url(
        "/api/v1/klines",
        &[
            ("symbol", ticker_info.ticker.to_string()),
            ("period", period.to_string()),
            ("start", start.to_string()),
            ("end", end.to_string()),
        ],
    )?;

    log::info!(
        "QMT fetch_kline_chunk start {} period={} range=({}..{})",
        ticker_info.ticker,
        period,
        start,
        end
    );
    let request_started_at = Instant::now();
    let response = qmt_bridge_http_client()
        .get(&url)
        .header(reqwest::header::ACCEPT, QMT_BRIDGE_MSGPACK_CONTENT_TYPE)
        .timeout(QMT_BRIDGE_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(AdapterError::from)?;
    let request_elapsed = request_started_at.elapsed();
    let status = response.status();
    let is_msgpack = qmt_response_is_msgpack(&response);
    let body_started_at = Instant::now();
    let bytes = response.bytes().await.map_err(AdapterError::from)?;
    let body_elapsed = body_started_at.elapsed();

    if !status.is_success() {
        return Err(AdapterError::http_status_failed(
            status,
            format!(
                "GET {url} failed: {}",
                qmt_response_body_to_error_detail(&bytes, is_msgpack)
            ),
        ));
    }

    if !is_msgpack {
        return Err(AdapterError::ParseError(format!(
            "QMT bridge returned unexpected content type for GET {url}"
        )));
    }

    let parse_started_at = Instant::now();
    let parsed: BridgeItemsResponse<QmtKlineBar> = qmt_decode_msgpack(&bytes)?;
    let parse_elapsed = parse_started_at.elapsed();
    log::info!(
        "QMT fetch_kline_chunk {} period={} range=({}..{}) status={} rows={} request_elapsed={:?} body_elapsed={:?} parse_elapsed={:?} total_elapsed={:?}",
        ticker_info.ticker,
        period,
        start,
        end,
        status,
        parsed.items.len(),
        request_elapsed,
        body_elapsed,
        parse_elapsed,
        total_started_at.elapsed()
    );
    Ok(parsed.items)
}

pub async fn fetch_order_panel_snapshot(
    ticker_info: TickerInfo,
) -> Result<OrderPanelSnapshot, AdapterError> {
    let url = qmt_bridge_http_url(
        "/api/v1/order/panel",
        &[("symbol", ticker_info.ticker.to_string())],
    )?;
    qmt_get_bridge(&url).await
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
    qmt_post_bridge(&url, &body).await
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
    qmt_post_bridge(&url, &body).await
}

pub async fn fetch_historical_oi(
    _ticker_info: TickerInfo,
    _range: Option<(u64, u64)>,
    _period: Timeframe,
) -> Result<Vec<OpenInterest>, AdapterError> {
    Ok(vec![])
}
