use super::*;
use chrono::NaiveDate;

fn sample_ticker_info() -> TickerInfo {
    TickerInfo::new(
        Ticker::new("600309.SH", super::super::Exchange::SSH),
        0.01,
        1.0,
        None,
    )
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
        last_price: 82.0,
        open: 0.0,
        high: 82.0,
        low: 82.0,
        last_close: 82.38,
        volume: 0,
        ask_price: Vec::new(),
        bid_price: Vec::new(),
        ask_vol: Vec::new(),
        bid_vol: Vec::new(),
    }
}

fn sample_depth_seed() -> Depth {
    let bids = [
        (99.96, 960.0),
        (99.97, 970.0),
        (99.98, 980.0),
        (99.99, 990.0),
        (100.00, 1000.0),
    ]
    .into_iter()
    .map(|(price, qty)| (Price::from_f32(price), Qty::from_f32(qty)))
    .collect();

    let asks = [
        (100.01, 1001.0),
        (100.02, 1002.0),
        (100.03, 1003.0),
        (100.04, 1004.0),
        (100.05, 1005.0),
    ]
    .into_iter()
    .map(|(price, qty)| (Price::from_f32(price), Qty::from_f32(qty)))
    .collect();

    Depth { bids, asks }
}

fn sample_live_depth_payload(time: u64) -> DepthPayload {
    DepthPayload {
        last_update_id: time,
        time,
        bids: vec![
            DeOrder {
                price: 100.00,
                qty: 2_000.0,
            },
            DeOrder {
                price: 99.99,
                qty: 1_999.0,
            },
        ],
        asks: vec![
            DeOrder {
                price: 100.01,
                qty: 2_001.0,
            },
            DeOrder {
                price: 100.02,
                qty: 2_002.0,
            },
        ],
    }
}

fn sample_book_tick(time: u64, bid_levels: &[(f32, f32)], ask_levels: &[(f32, f32)]) -> QmtTick {
    let mut tick = sample_tick(time);
    tick.volume = 100;
    tick.bid_price = bid_levels.iter().map(|(price, _)| *price).collect();
    tick.bid_vol = bid_levels.iter().map(|(_, qty)| *qty).collect();
    tick.ask_price = ask_levels.iter().map(|(price, _)| *price).collect();
    tick.ask_vol = ask_levels.iter().map(|(_, qty)| *qty).collect();
    tick
}

#[test]
fn synthesize_trade_uses_last_price_with_volume_qty() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.volume = 2_958;

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.08;
    current.volume = 3_175;
    current.bid_price = vec![82.06];
    current.ask_price = vec![82.08];

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);
    let trade = trades
        .first()
        .expect("expected synthetic trade from volume delta");

    assert_eq!(trade.time, current.time);
    assert_eq!(f32::from(trade.qty), 21_700.0);
    assert_eq!(trade.price.to_f32(), 82.08);
    assert!(!trade.is_sell);
}

#[test]
fn synthesize_trade_requires_current_last_price() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.last_price = 82.00;
    previous.volume = 2_958;

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 0.0;
    current.volume = 3_175;
    current.bid_price = vec![81.99];
    current.ask_price = vec![82.01];

    assert!(synthesize_trades_for_tick_pair(&previous, &current, ticker_info).is_empty());
}

#[test]
fn synthesize_trade_uses_previous_bid_for_sell_classification() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.volume = 100;

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.00;
    current.volume = 101;
    current.bid_price = vec![82.00];
    current.ask_price = vec![82.02];

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 1);
    assert!(trades[0].is_sell);
    assert_eq!(f32::from(trades[0].qty), 100.0);
}

#[test]
fn synthesize_trade_splits_inside_spread_even_on_uptick() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.volume = 100;

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.01;
    current.volume = 101;
    current.bid_price = vec![82.00];
    current.ask_price = vec![82.02];

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 2);
    assert!(!trades[0].is_sell);
    assert!(trades[1].is_sell);
    assert_eq!(f32::from(trades[0].qty), 50.0);
    assert_eq!(f32::from(trades[1].qty), 50.0);
}

#[test]
fn synthesize_trade_no_bba_ignores_stale_bba_and_uses_tick_rule() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.last_price = 82.03;
    previous.volume = 100;
    previous.bid_price = vec![82.00];
    previous.ask_price = vec![82.02];

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.02;
    current.volume = 101;

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 1);
    assert!(trades[0].is_sell);
    assert_eq!(f32::from(trades[0].qty), 100.0);
}

#[test]
fn synthesize_trade_uses_volume_delta_as_single_quantity_source() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.volume = 100;

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.02;
    current.volume = 101;
    current.bid_price = vec![82.00];
    current.ask_price = vec![82.02];

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 1);
    assert!(!trades[0].is_sell);
    assert_eq!(trades[0].price.to_f32(), 82.02);
    assert_eq!(f32::from(trades[0].qty), 100.0);
}

#[test]
fn synthesize_trade_no_bba_unchanged_price_splits() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.last_price = 82.00;
    previous.volume = 100;
    previous.bid_vol = vec![500.0];
    previous.ask_vol = vec![500.0];
    previous.bid_price = vec![81.99];
    previous.ask_price = vec![82.01];

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.00;
    current.volume = 101;
    current.bid_vol = vec![350.0];
    current.ask_vol = vec![490.0];

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 2);
    assert!(!trades[0].is_sell);
    assert!(trades[1].is_sell);
    assert_eq!(f32::from(trades[0].qty), 50.0);
    assert_eq!(f32::from(trades[1].qty), 50.0);
}

#[test]
fn synthesize_trade_splits_without_any_side_signal() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.last_price = 82.00;
    previous.volume = 100;

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.00;
    current.volume = 101;

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 2);
    assert!(!trades[0].is_sell);
    assert!(trades[1].is_sell);
    assert_eq!(f32::from(trades[0].qty), 50.0);
    assert_eq!(f32::from(trades[1].qty), 50.0);
}

#[test]
fn synthesize_trade_splits_inside_spread_even_on_downtick() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.last_price = 82.02;
    previous.volume = 100;

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.01;
    current.volume = 101;
    current.bid_price = vec![82.00];
    current.ask_price = vec![82.02];

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 2);
    assert!(!trades[0].is_sell);
    assert!(trades[1].is_sell);
    assert_eq!(f32::from(trades[0].qty), 50.0);
    assert_eq!(f32::from(trades[1].qty), 50.0);
}

#[test]
fn synthesize_trade_tick_rule_unchanged_inside_spread_splits() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.last_price = 82.01;
    previous.volume = 100;

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.01;
    current.volume = 101;
    current.bid_price = vec![82.00];
    current.ask_price = vec![82.02];

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 2);
    assert!(!trades[0].is_sell);
    assert!(trades[1].is_sell);
    assert_eq!(f32::from(trades[0].qty), 50.0);
    assert_eq!(f32::from(trades[1].qty), 50.0);
}

#[test]
fn synthesize_trade_no_bba_with_zero_volume_baseline_splits() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.last_price = 82.00;
    previous.volume = 0;
    previous.bid_price = vec![82.00];
    previous.ask_price = vec![82.02];

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.01;
    current.volume = 101;

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 2);
    assert!(!trades[0].is_sell);
    assert!(trades[1].is_sell);
    assert_eq!(f32::from(trades[0].qty), 5_050.0);
    assert_eq!(f32::from(trades[1].qty), 5_050.0);
}

#[test]
fn synthesize_trade_no_bba_uptick_buys() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.last_price = 82.00;
    previous.volume = 100;

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.01;
    current.volume = 101;

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 1);
    assert!(!trades[0].is_sell);
    assert_eq!(f32::from(trades[0].qty), 100.0);
}

#[test]
fn synthesize_trade_no_bba_downtick_sells() {
    let ticker_info = sample_ticker_info();
    let mut previous = sample_tick(1_775_525_401_000);
    previous.last_price = 82.02;
    previous.volume = 100;

    let mut current = sample_tick(1_775_525_404_000);
    current.last_price = 82.01;
    current.volume = 101;

    let trades = synthesize_trades_for_tick_pair(&previous, &current, ticker_info);

    assert_eq!(trades.len(), 1);
    assert!(trades[0].is_sell);
    assert_eq!(f32::from(trades[0].qty), 100.0);
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
    tick1.last_price = 82.00;
    tick1.high = 82.00;
    tick1.low = 82.00;

    let mut tick2 = sample_tick(start + 13_000);
    tick2.last_price = 82.09;
    tick2.high = 82.37;
    tick2.low = 81.80;

    let bars = aggregate_trades_to_klines(
        &trades,
        &[tick1, tick2],
        ticker_info,
        Timeframe::M5,
        start,
        end,
    )
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
    tick1.open = 81.80;
    tick1.last_price = 82.00;
    tick1.high = 82.00;
    tick1.low = 81.80;

    let mut tick2 = sample_tick(start + 13_000);
    tick2.open = 81.80;
    tick2.last_price = 82.09;
    tick2.high = 82.37;
    tick2.low = 81.80;

    let bars = aggregate_trades_to_klines(
        &trades,
        &[tick1, tick2],
        ticker_info,
        Timeframe::M5,
        start,
        end,
    )
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
fn supports_gapless_time_axis_timeframe_includes_heatmap_ms3000() {
    assert!(supports_gapless_time_axis_timeframe(
        Venue::SSH,
        Timeframe::MS3000
    ));
    assert!(supports_gapless_time_axis_timeframe(
        Venue::SSH,
        Timeframe::M3
    ));
}

#[test]
fn qmt_heatmap_gapless_axis_skips_lunch_gap() {
    let before_lunch = china_ms(2026, 4, 9, 11, 29, 57);
    let after_lunch = china_ms(2026, 4, 9, 13, 0, 0);

    assert_eq!(
        time_axis_bucket_offset(Venue::SSH, before_lunch, after_lunch, Timeframe::MS3000),
        Some(1)
    );
    assert_eq!(
        time_axis_bucket_offset(Venue::SSH, after_lunch, before_lunch, Timeframe::MS3000),
        Some(-1)
    );
    assert_eq!(
        time_axis_bucket_at_offset(Venue::SSH, before_lunch, Timeframe::MS3000, 1),
        Some(after_lunch)
    );
    assert_eq!(
        time_axis_bucket_at_offset(Venue::SSH, after_lunch, Timeframe::MS3000, -1),
        Some(before_lunch)
    );
}

#[test]
fn qmt_kline_seed_start_uses_same_day_session_start_mid_session() {
    let start = china_ms(2026, 4, 9, 10, 15, 0);
    let expected = china_ms(2026, 4, 9, 9, 30, 0);
    assert_eq!(qmt_kline_seed_start(Venue::SSH, start), Some(expected));
}

#[test]
fn qmt_kline_seed_start_uses_previous_trading_day_at_open() {
    let start_day = qmt_shift_trading_day(Venue::SSH, current_china_day().expect("china day"), -1)
        .expect("previous trading day");
    let expected_day = qmt_shift_trading_day(Venue::SSH, start_day, -1).expect("seed trading day");
    let start = china_ms(
        start_day.year(),
        start_day.month(),
        start_day.day(),
        9,
        30,
        0,
    );
    let expected = china_ms(
        expected_day.year(),
        expected_day.month(),
        expected_day.day(),
        9,
        30,
        0,
    );
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
    assert_eq!(
        qmt_bucket_start(Venue::SSH, lunch_tick, Timeframe::M30),
        None
    );
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
    tick1.volume = 100;
    tick1.last_price = 82.01;

    let mut tick2 = sample_tick(china_ms(2026, 4, 9, 9, 30, 2));
    tick2.volume = 120;
    tick2.last_price = 82.02;

    let mut tick3 = sample_tick(china_ms(2026, 4, 9, 9, 30, 3));
    tick3.volume = 140;
    tick3.last_price = 82.03;

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
    history_tick.volume = 120;
    history_tick.last_price = 82.02;

    let mut live_tick = sample_tick(ts);
    live_tick.volume = 130;
    live_tick.last_price = 82.03;

    let merged = merge_ticks(&[history_tick], vec![live_tick.clone()]);

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].time, ts);
    assert_eq!(merged[0].volume, live_tick.volume);
    assert_eq!(merged[0].last_price, live_tick.last_price);
}

#[test]
fn merge_ticks_keeps_latest_opening_zero_volume_baseline() {
    let zero_tick1 = sample_tick(china_ms(2026, 4, 9, 9, 15, 2));
    let zero_tick2 = sample_tick(china_ms(2026, 4, 9, 9, 24, 59));

    let mut live_tick = sample_tick(china_ms(2026, 4, 9, 9, 30, 2));
    live_tick.volume = 130;
    live_tick.last_price = 82.03;

    let merged = merge_ticks(&[zero_tick1], vec![zero_tick2.clone(), live_tick.clone()]);

    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].time, zero_tick2.time);
    assert_eq!(merged[0].volume, 0);
    assert_eq!(merged[1].time, live_tick.time);
    assert_eq!(merged[1].volume, live_tick.volume);
}

#[test]
fn merge_ticks_drops_zero_volume_ticks_after_traded_volume() {
    let mut traded_tick = sample_tick(china_ms(2026, 4, 9, 9, 30, 2));
    traded_tick.volume = 130;
    traded_tick.last_price = 82.03;

    let zero_tick = sample_tick(china_ms(2026, 4, 9, 9, 30, 5));

    let merged = merge_ticks(&[traded_tick.clone()], vec![zero_tick]);

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].time, traded_tick.time);
    assert_eq!(merged[0].volume, traded_tick.volume);
}

#[test]
fn synthetic_trade_state_uses_zero_volume_opening_baseline() {
    let ticker_info = sample_ticker_info();
    let mut trade_state = SyntheticTradeState::default();

    let zero_tick = sample_tick(china_ms(2026, 4, 9, 9, 15, 2));
    assert!(trade_state.update(zero_tick, ticker_info).is_empty());
    assert_eq!(
        trade_state.previous_tick.as_ref().map(|tick| tick.volume),
        Some(0)
    );

    let mut first_trade_tick = sample_tick(china_ms(2026, 4, 9, 9, 25, 2));
    first_trade_tick.volume = 4_494;
    first_trade_tick.last_price = 85.0;
    let opening_trades = trade_state.update(first_trade_tick, ticker_info);

    assert_eq!(opening_trades.len(), 2);
    assert_eq!(f32::from(opening_trades[0].qty), 224_700.0);
    assert_eq!(f32::from(opening_trades[1].qty), 224_700.0);

    let zero_after_open = sample_tick(china_ms(2026, 4, 9, 9, 25, 4));
    assert!(trade_state.update(zero_after_open, ticker_info).is_empty());

    let mut next_tick = sample_tick(china_ms(2026, 4, 9, 9, 25, 5));
    next_tick.volume = 4_594;
    next_tick.last_price = 85.0;
    let trades = trade_state.update(next_tick, ticker_info);

    assert_eq!(trades.len(), 2);
    assert_eq!(f32::from(trades[0].qty), 5_000.0);
    assert_eq!(f32::from(trades[1].qty), 5_000.0);
}

#[test]
fn current_day_history_ready_only_after_merge() {
    let ticker_info = sample_ticker_info();
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

    let mut live_tick = sample_tick(timestamp);
    live_tick.volume = 100;
    live_tick.last_price = 82.01;
    cache_live_tick(ticker, &live_tick);
    assert!(!current_day_history_ready(ticker, day));

    let _ = merge_current_day_history_and_live(ticker_info, day, vec![live_tick]);
    assert!(current_day_history_ready(ticker, day));
}

#[test]
fn current_day_history_snapshot_is_fresh_right_after_merge() {
    let ticker_info = sample_ticker_info();
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

    let mut tick = sample_tick(timestamp);
    tick.volume = 100;
    tick.last_price = 82.01;

    let merged = merge_current_day_history_and_live(ticker_info, day, vec![tick.clone()]);
    let fresh = current_day_history_snapshot_if_fresh(ticker, day, Duration::from_secs(1))
        .expect("fresh current-day history snapshot");

    assert_eq!(merged.len(), 1);
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].time, tick.time);
}

#[test]
fn current_day_history_snapshot_expires_after_ttl() {
    let ticker_info = sample_ticker_info();
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

    let mut tick = sample_tick(timestamp);
    tick.volume = 100;
    tick.last_price = 82.01;
    let _ = merge_current_day_history_and_live(ticker_info, day, vec![tick]);

    if let Ok(mut cache) = CURRENT_DAY_TICK_CACHE.write()
        && let Some(entry) = cache.get_mut(&ticker)
    {
        entry.last_history_loaded_at = Some(Instant::now() - Duration::from_secs(5));
    }

    assert!(current_day_history_snapshot_if_fresh(ticker, day, Duration::from_secs(1)).is_none());
}

#[test]
fn current_day_history_merge_stores_depth_seed_from_pure_history() {
    let ticker_info = sample_ticker_info();
    let ticker = ticker_info.ticker;
    let day = current_china_day().expect("current china day");
    let ts = china_ms(day.year(), day.month(), day.day(), 10, 0, 0);

    if let Ok(mut cache) = CURRENT_DAY_TICK_CACHE.write() {
        cache.clear();
    }

    let mut history_tick = sample_tick(ts);
    history_tick.volume = 100;
    history_tick.bid_price = vec![100.00, 99.99, 99.98, 99.97, 99.96];
    history_tick.ask_price = vec![100.01, 100.02, 100.03, 100.04, 100.05];
    history_tick.bid_vol = vec![1000.0, 990.0, 980.0, 970.0, 960.0];
    history_tick.ask_vol = vec![1001.0, 1002.0, 1003.0, 1004.0, 1005.0];

    let _ = merge_current_day_history_and_live(ticker_info, day, vec![history_tick]);
    let depth_seed = current_day_history_depth_seed(ticker, day).expect("depth seed");

    assert_eq!(depth_seed.bids.len(), 5);
    assert_eq!(depth_seed.asks.len(), 5);
    let (best_bid, best_bid_qty) = depth_seed.bids.last_key_value().expect("best bid");
    let (worst_ask, worst_ask_qty) = depth_seed.asks.last_key_value().expect("worst ask");
    assert!((best_bid.to_f32() - 100.00).abs() < 0.001);
    assert_eq!(*best_bid_qty, Qty::from_f32(1000.0));
    assert!((worst_ask.to_f32() - 100.05).abs() < 0.001);
    assert_eq!(*worst_ask_qty, Qty::from_f32(1005.0));
}

#[test]
fn synthesize_qmt_depth_payload_preserves_deeper_history_levels() {
    let payload = super::streams::synthesize_qmt_depth_payload(
        sample_live_depth_payload(1_000),
        &sample_depth_seed(),
        300,
    );

    assert_eq!(payload.bids.len(), 5);
    assert_eq!(payload.asks.len(), 5);
    assert_eq!(payload.bids[0].price, 100.00);
    assert_eq!(payload.bids[0].qty, 2_000.0);
    assert_eq!(payload.bids[1].price, 99.99);
    assert_eq!(payload.bids[1].qty, 1_999.0);
    assert!((payload.bids[2].price - 99.98).abs() < 0.001);
    assert!((payload.asks[0].price - 100.01).abs() < 0.001);
    assert_eq!(payload.asks[0].qty, 2_001.0);
    assert!((payload.asks[1].price - 100.02).abs() < 0.001);
    assert_eq!(payload.asks[1].qty, 2_002.0);
    assert!((payload.asks[2].price - 100.03).abs() < 0.001);
}

#[test]
fn synthesize_qmt_depth_payload_caps_sides_to_300_levels() {
    let deep_bids = (0..360)
        .map(|offset| {
            let price = 100.00 - (offset as f32 * 0.01);
            (
                Price::from_f32(price),
                Qty::from_f32(1_000.0 - offset as f32),
            )
        })
        .collect();
    let deep_asks = (0..360)
        .map(|offset| {
            let price = 100.01 + (offset as f32 * 0.01);
            (
                Price::from_f32(price),
                Qty::from_f32(1_000.0 - offset as f32),
            )
        })
        .collect();
    let baseline = Depth {
        bids: deep_bids,
        asks: deep_asks,
    };

    let payload = super::streams::synthesize_qmt_depth_payload(
        sample_live_depth_payload(1_000),
        &baseline,
        300,
    );

    assert_eq!(payload.bids.len(), 300);
    assert_eq!(payload.asks.len(), 300);
}

#[test]
fn synthesize_qmt_depth_payload_respects_custom_level_limit() {
    let baseline = Depth {
        bids: (0..20)
            .map(|offset| {
                (
                    Price::from_f32(100.0 - (offset as f32 * 0.01)),
                    Qty::from_f32(1_000.0 - offset as f32),
                )
            })
            .collect(),
        asks: (0..20)
            .map(|offset| {
                (
                    Price::from_f32(100.01 + (offset as f32 * 0.01)),
                    Qty::from_f32(1_000.0 - offset as f32),
                )
            })
            .collect(),
    };

    let payload = super::streams::synthesize_qmt_depth_payload(
        sample_live_depth_payload(1_000),
        &baseline,
        7,
    );

    assert_eq!(payload.bids.len(), 7);
    assert_eq!(payload.asks.len(), 7);
}

#[test]
fn build_depth_history_from_ticks_preserves_deeper_history_levels() {
    let ticker_info = sample_ticker_info();
    let ticks = vec![
        sample_book_tick(
            1_000,
            &[(100.00, 1000.0), (99.99, 999.0), (99.98, 998.0)],
            &[(100.01, 1001.0), (100.02, 1002.0), (100.03, 1003.0)],
        ),
        sample_book_tick(
            4_000,
            &[(100.00, 2000.0), (99.99, 1999.0)],
            &[(100.01, 2001.0), (100.02, 2002.0)],
        ),
    ];

    let depths = build_depth_history_from_ticks(&ticks, ticker_info, Some(300));
    let latest = depths.last().expect("latest depth snapshot").1.clone();
    let bid_levels = latest
        .bids
        .iter()
        .map(|(price, qty)| (price.to_f32(), qty.to_f32_lossy()))
        .collect::<Vec<_>>();
    let ask_levels = latest
        .asks
        .iter()
        .map(|(price, qty)| (price.to_f32(), qty.to_f32_lossy()))
        .collect::<Vec<_>>();

    assert_eq!(latest.bids.len(), 3);
    assert_eq!(latest.asks.len(), 3);
    assert!((bid_levels[2].0 - 100.00).abs() < 0.001);
    assert!((bid_levels[2].1 - 2000.0).abs() < 0.001);
    assert!((bid_levels[0].0 - 99.98).abs() < 0.001);
    assert!((bid_levels[0].1 - 998.0).abs() < 0.001);
    assert!((ask_levels[0].0 - 100.01).abs() < 0.001);
    assert!((ask_levels[0].1 - 2001.0).abs() < 0.001);
    assert!((ask_levels[2].0 - 100.03).abs() < 0.001);
    assert!((ask_levels[2].1 - 1003.0).abs() < 0.001);
}

#[test]
fn build_live_kline_from_ticks_reconstructs_current_bucket() {
    let ticker_info = sample_ticker_info();
    let bucket_start = china_ms(2026, 4, 9, 10, 0, 0);

    let mut seed_tick = sample_tick(bucket_start - 1_000);
    seed_tick.last_price = 82.10;
    seed_tick.volume = 1_000;
    seed_tick.bid_price = vec![81.99];
    seed_tick.ask_price = vec![82.00];

    let mut tick1 = sample_tick(bucket_start + 1_000);
    tick1.open = 82.00;
    tick1.last_price = 82.00;
    tick1.high = 82.00;
    tick1.low = 82.00;
    tick1.volume = 1_010;
    tick1.bid_price = vec![82.06];
    tick1.ask_price = vec![82.08];

    let mut tick2 = sample_tick(bucket_start + 4_000);
    tick2.open = 82.00;
    tick2.last_price = 82.08;
    tick2.high = 82.08;
    tick2.low = 82.00;
    tick2.volume = 1_030;

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
    tick1.open = 82.00;
    tick1.last_price = 82.02;
    tick1.volume = 1_000;
    tick1.bid_price = vec![82.04];
    tick1.ask_price = vec![82.06];

    let mut tick2 = sample_tick(second_bucket + 2_000);
    tick2.open = 82.05;
    tick2.last_price = 82.06;
    tick2.high = 82.06;
    tick2.low = 82.05;
    tick2.volume = 1_020;

    let kline =
        build_live_kline_from_ticks(ticker_info, Timeframe::M30, &tick2, &[tick1, tick2.clone()])
            .expect("expected next bucket kline");

    assert_eq!(kline.time, second_bucket);
    assert_eq!(kline.close.to_f32(), 82.06);
}
