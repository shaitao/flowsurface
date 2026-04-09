use super::*;

pub(super) async fn ensure_trading_calendar(
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

        if let Some(price) = tick.valid_last_close().or_else(|| tick.valid_last_price()) {
            seeds.insert(day, price);
        }
    }

    seeds
}

pub(super) fn synthesize_trades_from_ticks(
    ticks: &[QmtTick],
    ticker_info: TickerInfo,
) -> Vec<Trade> {
    ticks
        .windows(2)
        .flat_map(|pair| {
            let [previous_tick, current_tick] = pair else {
                return Vec::new();
            };
            synthesize_trades_for_tick_pair(previous_tick, current_tick, ticker_info)
        })
        .collect()
}

pub(super) fn aggregate_trades_to_klines(
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

        if let Some(last_price) = tick.valid_last_price() {
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
        let day_changed =
            china_trading_day(previous_tick.time) != china_trading_day(current_tick.time);

        if let Some(current_high) = current_tick.valid_high() {
            let high_increased = day_changed
                || previous_tick.valid_high().is_none()
                || current_high > previous_tick.high + f32::EPSILON;
            if high_increased {
                bar.high = bar.high.max(current_high);
            }
        }

        if let Some(current_low) = current_tick.valid_low() {
            let low_decreased = day_changed
                || previous_tick.valid_low().is_none()
                || current_low + f32::EPSILON < previous_tick.low;
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

        if let Some(open_price) = tick.valid_open() {
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

fn split_synthetic_qty(total_qty: Qty) -> (Qty, Qty) {
    let sell_qty = Qty::from_units(total_qty.units.div_euclid(2));
    let buy_qty = total_qty - sell_qty;
    (buy_qty, sell_qty)
}

fn tick_rule_side(previous_tick: &QmtTick, current_tick: &QmtTick) -> SyntheticTradeSide {
    match (
        previous_tick.valid_last_price(),
        current_tick.valid_last_price(),
    ) {
        (Some(prev), Some(curr)) if curr > prev => SyntheticTradeSide::Buy,
        (Some(prev), Some(curr)) if curr < prev => SyntheticTradeSide::Sell,
        _ => SyntheticTradeSide::Split,
    }
}

fn classify_with_bba(
    previous_tick: &QmtTick,
    current_tick: &QmtTick,
    bid1: f32,
    ask1: f32,
    synthetic_price: f32,
    warning_enabled: bool,
) -> SyntheticTradeSide {
    if ask1 <= bid1 {
        if warning_enabled {
            log_qmt_synthetic_warning(
                &INVALID_TOP_OF_BOOK_WARN_COUNT,
                "invalid top of book ask1<=bid1",
                || {
                    format!(
                        "bid1={bid1} ask1={ask1} previous_tick={previous_tick:?} current_tick={current_tick:?}"
                    )
                },
            );
        }
        return SyntheticTradeSide::Split;
    }
    if synthetic_price >= ask1 {
        return SyntheticTradeSide::Buy;
    }
    if synthetic_price <= bid1 {
        return SyntheticTradeSide::Sell;
    }
    SyntheticTradeSide::Split
}

fn classify_synthetic_trade_side(
    previous_tick: &QmtTick,
    current_tick: &QmtTick,
    synthetic_price: f32,
) -> SyntheticTradeSide {
    let warning_enabled = qmt_synthetic_warning_enabled(previous_tick, current_tick);
    let current_bid1 = current_tick.valid_bid1();
    let current_ask1 = current_tick.valid_ask1();

    if let (Some(bid1), Some(ask1)) = (current_bid1, current_ask1) {
        return classify_with_bba(
            previous_tick,
            current_tick,
            bid1,
            ask1,
            synthetic_price,
            warning_enabled,
        );
    }

    if !qmt_tick_has_traded_volume(previous_tick) {
        return SyntheticTradeSide::Split;
    }

    tick_rule_side(previous_tick, current_tick)
}

fn synthetic_trade_qty_from_volume(previous_tick: &QmtTick, current_tick: &QmtTick) -> Option<f32> {
    let warning_enabled = qmt_synthetic_warning_enabled(previous_tick, current_tick);
    let day_changed = china_trading_day(previous_tick.time) != china_trading_day(current_tick.time);

    if current_tick.volume < previous_tick.volume {
        if !day_changed {
            if warning_enabled {
                log_qmt_synthetic_warning(
                    &VOLUME_REGRESSION_WARN_COUNT,
                    "volume regressed inside same trading day",
                    || format!("previous_tick={previous_tick:?} current_tick={current_tick:?}"),
                );
            }
            return None;
        }
        return day_changed
            .then_some(current_tick.volume)
            .filter(|volume| *volume > 0)
            .map(|volume| volume as f32 * QMT_VOLUME_LOT_SIZE);
    }

    (current_tick.volume > previous_tick.volume)
        .then_some(current_tick.volume - previous_tick.volume)
        .map(|delta| delta as f32 * QMT_VOLUME_LOT_SIZE)
}

pub(super) fn synthesize_trades_for_tick_pair(
    previous_tick: &QmtTick,
    current_tick: &QmtTick,
    ticker_info: TickerInfo,
) -> Vec<Trade> {
    let warning_enabled = qmt_synthetic_warning_enabled(previous_tick, current_tick);
    let Some(qty_raw) = synthetic_trade_qty_from_volume(previous_tick, current_tick) else {
        return Vec::new();
    };
    let Some(raw_price) = current_tick.valid_last_price() else {
        if warning_enabled {
            log_qmt_synthetic_warning(
                &MISSING_LAST_PRICE_WARN_COUNT,
                "missing current last_price for positive volume delta",
                || format!("previous_tick={previous_tick:?} current_tick={current_tick:?}"),
            );
        }
        return Vec::new();
    };

    let price = Price::from_f32(raw_price).round_to_min_tick(ticker_info.min_ticksize);
    let qty = Qty::from_f32(qty_raw).round_to_min_qty(ticker_info.min_qty);

    if qty.is_zero() {
        if warning_enabled {
            log_qmt_synthetic_warning(&ZERO_QTY_WARN_COUNT, "rounded quantity to zero", || {
                format!(
                    "qty_raw={qty_raw} ticker_info={ticker_info:?} previous_tick={previous_tick:?} current_tick={current_tick:?}"
                )
            });
        }
        return Vec::new();
    }

    let side = classify_synthetic_trade_side(previous_tick, current_tick, raw_price);

    match side {
        SyntheticTradeSide::Buy => vec![Trade {
            time: current_tick.time,
            is_sell: false,
            price,
            qty,
        }],
        SyntheticTradeSide::Sell => vec![Trade {
            time: current_tick.time,
            is_sell: true,
            price,
            qty,
        }],
        SyntheticTradeSide::Split => {
            let (buy_qty, sell_qty) = split_synthetic_qty(qty);
            let mut trades = Vec::with_capacity(2);

            if !buy_qty.is_zero() {
                trades.push(Trade {
                    time: current_tick.time,
                    is_sell: false,
                    price,
                    qty: buy_qty,
                });
            }
            if !sell_qty.is_zero() {
                trades.push(Trade {
                    time: current_tick.time,
                    is_sell: true,
                    price,
                    qty: sell_qty,
                });
            }

            trades
        }
    }
}
