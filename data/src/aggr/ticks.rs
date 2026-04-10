use crate::aggr;
use crate::chart::kline::{ClusterKind, KlineTrades, NPoc};
use exchange::unit::Qty;
use exchange::unit::price::{Price, PriceStep};
use exchange::{Kline, Trade, Volume};

use chrono::FixedOffset;
use std::collections::BTreeMap;

fn china_trading_day(timestamp_ms: u64) -> Option<chrono::NaiveDate> {
    let offset = FixedOffset::east_opt(8 * 60 * 60)?;
    chrono::DateTime::from_timestamp_millis(timestamp_ms as i64)
        .map(|dt| dt.with_timezone(&offset).date_naive())
}

fn trade_session_day(
    split: aggr::TradeSessionSplit,
    timestamp_ms: u64,
) -> Option<chrono::NaiveDate> {
    match split {
        aggr::TradeSessionSplit::None => None,
        aggr::TradeSessionSplit::ChinaTradingDay => china_trading_day(timestamp_ms),
    }
}

#[derive(Debug, Clone)]
pub struct TickAccumulation {
    pub tick_count: usize,
    pub total_volume: Qty,
    pub kline: Kline,
    pub footprint: KlineTrades,
}

impl TickAccumulation {
    pub fn new(trade: &Trade, step: PriceStep) -> Self {
        let mut footprint = KlineTrades::new();
        footprint.add_trade_to_nearest_bin(trade, step);

        let kline = Kline {
            time: trade.time,
            open: trade.price,
            high: trade.price,
            low: trade.price,
            close: trade.price,
            volume: Volume::empty_buy_sell().add_trade_qty(trade.is_sell, trade.qty),
        };

        Self {
            tick_count: 1,
            total_volume: trade.qty,
            kline,
            footprint,
        }
    }

    pub fn update_with_trade(&mut self, trade: &Trade, step: PriceStep) {
        self.tick_count += 1;
        self.total_volume += trade.qty;
        self.kline.high = self.kline.high.max(trade.price);
        self.kline.low = self.kline.low.min(trade.price);
        self.kline.close = trade.price;

        self.kline.volume = self.kline.volume.add_trade_qty(trade.is_sell, trade.qty);

        self.add_trade(trade, step);
    }

    fn add_trade(&mut self, trade: &Trade, step: PriceStep) {
        self.footprint.add_trade_to_nearest_bin(trade, step);
    }

    pub fn max_cluster_qty(&self, cluster_kind: ClusterKind, highest: Price, lowest: Price) -> Qty {
        self.footprint
            .max_cluster_qty(cluster_kind, highest, lowest)
    }

    pub fn is_full(&self, interval: aggr::TradeAggregation) -> bool {
        match interval {
            aggr::TradeAggregation::Tick(count) => self.tick_count >= count.0 as usize,
            aggr::TradeAggregation::Volume(threshold) => self.total_volume >= threshold.raw_qty(),
        }
    }

    pub fn poc_price(&self) -> Option<Price> {
        self.footprint.poc_price()
    }

    pub fn set_poc_status(&mut self, status: NPoc) {
        self.footprint.set_poc_status(status);
    }

    pub fn calculate_poc(&mut self) {
        self.footprint.calculate_poc();
    }
}

pub struct TickAggr {
    pub datapoints: Vec<TickAccumulation>,
    pub interval: aggr::TradeAggregation,
    pub tick_size: PriceStep,
    pub session_split: aggr::TradeSessionSplit,
}

impl TickAggr {
    pub fn new(
        interval: aggr::TradeAggregation,
        tick_size: PriceStep,
        raw_trades: &[Trade],
        session_split: aggr::TradeSessionSplit,
    ) -> Self {
        let mut tick_aggr = Self {
            datapoints: Vec::new(),
            interval,
            tick_size,
            session_split,
        };

        if !raw_trades.is_empty() {
            tick_aggr.insert_trades(raw_trades);
        }

        tick_aggr
    }

    pub fn change_tick_size(&mut self, tick_size: PriceStep, raw_trades: &[Trade]) {
        self.tick_size = tick_size;

        self.datapoints.clear();

        if !raw_trades.is_empty() {
            self.insert_trades(raw_trades);
        }
    }

    /// return latest data point and its index
    pub fn latest_dp(&self) -> Option<(&TickAccumulation, usize)> {
        self.datapoints
            .last()
            .map(|dp| (dp, self.datapoints.len() - 1))
    }

    pub fn volume_data(&self) -> BTreeMap<u64, exchange::Volume> {
        self.into()
    }

    pub fn insert_trades(&mut self, buffer: &[Trade]) {
        let mut updated_indices = Vec::new();

        for trade in buffer {
            if self.datapoints.is_empty() {
                self.datapoints
                    .push(TickAccumulation::new(trade, self.tick_size));
                updated_indices.push(0);
            } else {
                let last_idx = self.datapoints.len() - 1;
                let last_trade_time = self.datapoints[last_idx].kline.time;
                let session_changed = trade_session_day(self.session_split, last_trade_time)
                    != trade_session_day(self.session_split, trade.time);

                if self.datapoints[last_idx].is_full(self.interval) || session_changed {
                    self.datapoints
                        .push(TickAccumulation::new(trade, self.tick_size));
                    updated_indices.push(self.datapoints.len() - 1);
                } else {
                    self.datapoints[last_idx].update_with_trade(trade, self.tick_size);
                    if !updated_indices.contains(&last_idx) {
                        updated_indices.push(last_idx);
                    }
                }
            }
        }

        for idx in updated_indices {
            if idx < self.datapoints.len() {
                self.datapoints[idx].calculate_poc();
            }
        }

        self.update_poc_status();
    }

    pub fn update_poc_status(&mut self) {
        let updates = self
            .datapoints
            .iter()
            .enumerate()
            .filter_map(|(idx, dp)| dp.poc_price().map(|price| (idx, price)))
            .collect::<Vec<_>>();

        let total_points = self.datapoints.len();

        for (current_idx, poc_price) in updates {
            let mut npoc = NPoc::default();

            for next_idx in (current_idx + 1)..total_points {
                let next_dp = &self.datapoints[next_idx];

                let next_dp_low = next_dp.kline.low.round_to_side_step(true, self.tick_size);
                let next_dp_high = next_dp.kline.high.round_to_side_step(false, self.tick_size);

                if next_dp_low <= poc_price && next_dp_high >= poc_price {
                    // on render we reverse the order of the points
                    // as it is easier to just take the idx=0 as latest candle for coords
                    let reversed_idx = (total_points - 1) - next_idx;
                    npoc.filled(reversed_idx as u64);
                    break;
                } else {
                    npoc.unfilled();
                }
            }

            if current_idx < total_points {
                let data_point = &mut self.datapoints[current_idx];
                data_point.set_poc_status(npoc);
            }
        }
    }

    pub fn min_max_price_in_range_prices(
        &self,
        earliest: usize,
        latest: usize,
    ) -> Option<(Price, Price)> {
        if earliest > latest {
            return None;
        }

        let mut min_p: Option<Price> = None;
        let mut max_p: Option<Price> = None;

        self.datapoints
            .iter()
            .rev()
            .enumerate()
            .filter(|(idx, _)| *idx >= earliest && *idx <= latest)
            .for_each(|(_, dp)| {
                let low = dp.kline.low;
                let high = dp.kline.high;

                min_p = Some(match min_p {
                    Some(value) => value.min(low),
                    None => low,
                });
                max_p = Some(match max_p {
                    Some(value) => value.max(high),
                    None => high,
                });
            });

        match (min_p, max_p) {
            (Some(low), Some(high)) => Some((low, high)),
            _ => None,
        }
    }

    pub fn min_max_price_in_range(&self, earliest: usize, latest: usize) -> Option<(f32, f32)> {
        self.min_max_price_in_range_prices(earliest, latest)
            .map(|(min_p, max_p)| (min_p.to_f32(), max_p.to_f32()))
    }

    pub fn max_qty_idx_range(
        &self,
        cluster_kind: ClusterKind,
        earliest: usize,
        latest: usize,
        highest: Price,
        lowest: Price,
    ) -> Qty {
        let mut max_cluster_qty: Qty = Qty::default();

        self.datapoints
            .iter()
            .rev()
            .enumerate()
            .filter(|(index, _)| *index <= latest && *index >= earliest)
            .for_each(|(_, dp)| {
                max_cluster_qty =
                    max_cluster_qty.max(dp.max_cluster_qty(cluster_kind, highest, lowest));
            });

        max_cluster_qty
    }
}

impl From<&TickAggr> for BTreeMap<u64, exchange::Volume> {
    /// Converts datapoints into a map of timestamps and volume data
    fn from(tick_aggr: &TickAggr) -> Self {
        tick_aggr
            .datapoints
            .iter()
            .enumerate()
            .map(|(idx, dp)| (idx as u64, dp.kline.volume))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::TickAggr;
    use crate::aggr::{TickCount, TradeAggregation, TradeSessionSplit, VolumeThreshold};
    use chrono::{FixedOffset, TimeZone};
    use exchange::Trade;
    use exchange::unit::{Price, Qty, price::PriceStep};

    fn trade(time: u64, price: f32, qty: i64, is_sell: bool) -> Trade {
        Trade {
            time,
            is_sell,
            price: Price::from_f32(price),
            qty: Qty::from_units(qty * 10i64.pow(Qty::QTY_SCALE as u32)),
        }
    }

    fn china_timestamp(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> u64 {
        let offset = FixedOffset::east_opt(8 * 60 * 60).expect("china offset");
        offset
            .with_ymd_and_hms(y, m, d, h, min, s)
            .single()
            .expect("valid china datetime")
            .timestamp_millis() as u64
    }

    #[test]
    fn volume_threshold_bars_roll_on_next_trade_after_overshoot() {
        let trades = vec![
            trade(1_000, 100.0, 100, false),
            trade(2_000, 101.0, 150, false),
            trade(3_000, 99.0, 70, true),
            trade(4_000, 102.0, 100, false),
        ];

        let aggr = TickAggr::new(
            TradeAggregation::Volume(VolumeThreshold(3)),
            PriceStep::from_f32(0.01),
            &trades,
            TradeSessionSplit::None,
        );

        assert_eq!(aggr.datapoints.len(), 2);

        let first = &aggr.datapoints[0];
        assert_eq!(first.tick_count, 3);
        assert_eq!(
            first.total_volume,
            Qty::from_units(320 * 10i64.pow(Qty::QTY_SCALE as u32))
        );
        assert_eq!(first.kline.open, Price::from_f32(100.0));
        assert_eq!(first.kline.close, Price::from_f32(99.0));

        let second = &aggr.datapoints[1];
        assert_eq!(second.tick_count, 1);
        assert_eq!(
            second.total_volume,
            Qty::from_units(100 * 10i64.pow(Qty::QTY_SCALE as u32))
        );
        assert_eq!(second.kline.open, Price::from_f32(102.0));
        assert_eq!(second.kline.close, Price::from_f32(102.0));
    }

    #[test]
    fn tick_count_bars_still_group_by_trade_count() {
        let trades = vec![
            trade(1_000, 100.0, 1, false),
            trade(2_000, 101.0, 1, false),
            trade(3_000, 99.0, 1, true),
        ];

        let aggr = TickAggr::new(
            TradeAggregation::Tick(TickCount(2)),
            PriceStep::from_f32(0.01),
            &trades,
            TradeSessionSplit::None,
        );

        assert_eq!(aggr.datapoints.len(), 2);
        assert_eq!(aggr.datapoints[0].tick_count, 2);
        assert_eq!(aggr.datapoints[1].tick_count, 1);
    }

    #[test]
    fn volume_threshold_bars_reset_on_china_trading_day_boundary() {
        let trades = vec![
            trade(china_timestamp(2026, 4, 9, 14, 59, 57), 100.0, 100, false),
            trade(china_timestamp(2026, 4, 9, 14, 59, 59), 101.0, 100, false),
            trade(china_timestamp(2026, 4, 10, 9, 30, 0), 102.0, 100, true),
        ];

        let aggr = TickAggr::new(
            TradeAggregation::Volume(VolumeThreshold(10)),
            PriceStep::from_f32(0.01),
            &trades,
            TradeSessionSplit::ChinaTradingDay,
        );

        assert_eq!(aggr.datapoints.len(), 2);
        assert_eq!(aggr.datapoints[0].tick_count, 2);
        assert_eq!(aggr.datapoints[0].kline.open, Price::from_f32(100.0));
        assert_eq!(aggr.datapoints[0].kline.close, Price::from_f32(101.0));
        assert_eq!(aggr.datapoints[1].tick_count, 1);
        assert_eq!(aggr.datapoints[1].kline.open, Price::from_f32(102.0));
        assert_eq!(aggr.datapoints[1].kline.close, Price::from_f32(102.0));
    }
}
