use crate::chart::{
    Caches, Message, ViewState,
    indicator::{
        indicator_row,
        kline::KlineIndicatorImpl,
        plot::{
            PlotTooltip,
            bar::{BarClass, BarPlot},
        },
    },
};

use data::chart::{PlotData, kline::KlineDataPoint};
use data::util::format_with_commas;
use exchange::{Kline, Trade, Volume, unit::qty::Qty};

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

#[derive(Debug, Clone, Copy)]
struct VolumePoint {
    total: Qty,
    buy_sell: Option<(Qty, Qty)>,
}

impl VolumePoint {
    fn from_volume(volume: Volume) -> Self {
        Self {
            total: volume.total(),
            buy_sell: volume.buy_sell(),
        }
    }

    fn from_kline_dp(dp: &KlineDataPoint) -> Self {
        if dp.footprint.trades.is_empty() {
            return Self::from_volume(dp.kline.volume);
        }

        let (buy, sell) = dp
            .footprint
            .trades
            .values()
            .fold((Qty::ZERO, Qty::ZERO), |(buy, sell), group| {
                (buy + group.buy_qty, sell + group.sell_qty)
            });

        Self {
            total: buy + sell,
            buy_sell: Some((buy, sell)),
        }
    }

    fn delta(&self) -> Option<Qty> {
        self.buy_sell.map(|(buy, sell)| buy - sell)
    }
}

pub struct VolumeIndicator {
    cache: Caches,
    data: BTreeMap<u64, VolumePoint>,
}

impl VolumeIndicator {
    pub fn new() -> Self {
        Self {
            cache: Caches::default(),
            data: BTreeMap::new(),
        }
    }

    fn indicator_elem<'a>(
        &'a self,
        main_chart: &'a ViewState,
        visible_range: RangeInclusive<u64>,
    ) -> iced::Element<'a, Message> {
        let tooltip = |point: &VolumePoint, _next: Option<&VolumePoint>| {
            if let Some((buy, sell)) = point.buy_sell {
                let delta = point.delta().unwrap_or(Qty::ZERO);
                let total = f32::from(point.total);
                let delta_ratio = if total > 0.0 {
                    (f32::from(delta) / total) * 100.0
                } else {
                    0.0
                };

                let total_t = format!("Total Volume: {}", format_with_commas(total));
                let delta_t = format!("Delta: {}", format_with_commas(f32::from(delta)));
                let delta_ratio_t = format!("Delta Ratio: {delta_ratio:+.2}%");
                let buy_t = format!("Buy Volume: {}", format_with_commas(f32::from(buy)));
                let sell_t = format!("Sell Volume: {}", format_with_commas(f32::from(sell)));
                PlotTooltip::new(format!(
                    "{delta_t}\n{delta_ratio_t}\n{total_t}\n{buy_t}\n{sell_t}"
                ))
            } else {
                PlotTooltip::new(format!(
                    "Total Volume: {}",
                    format_with_commas(f32::from(point.total))
                ))
            }
        };

        let bar_kind = |point: &VolumePoint| {
            if let Some((buy, sell)) = point.buy_sell {
                BarClass::Overlay {
                    overlay: f32::from(buy) - f32::from(sell),
                }
            } else {
                BarClass::Single
            }
        };

        let value_fn = |point: &VolumePoint| f32::from(point.total);

        let plot = BarPlot::new(value_fn, bar_kind)
            .bar_width_factor(0.9)
            .with_tooltip(tooltip);

        indicator_row(main_chart, &self.cache, plot, &self.data, visible_range)
    }
}

impl KlineIndicatorImpl for VolumeIndicator {
    fn clear_all_caches(&mut self) {
        self.cache.clear_all();
    }

    fn clear_crosshair_caches(&mut self) {
        self.cache.clear_crosshair();
    }

    fn element<'a>(
        &'a self,
        chart: &'a ViewState,
        visible_range: RangeInclusive<u64>,
    ) -> iced::Element<'a, Message> {
        self.indicator_elem(chart, visible_range)
    }

    fn rebuild_from_source(&mut self, source: &PlotData<KlineDataPoint>) {
        match source {
            PlotData::TimeBased(timeseries) => {
                self.data = timeseries
                    .datapoints
                    .iter()
                    .map(|(time, dp)| (*time, VolumePoint::from_kline_dp(dp)))
                    .collect();
            }
            PlotData::TickBased(tickseries) => {
                self.data = tickseries
                    .volume_data()
                    .into_iter()
                    .map(|(time, volume)| (time, VolumePoint::from_volume(volume)))
                    .collect();
            }
        }
        self.clear_all_caches();
    }

    fn on_insert_klines(&mut self, klines: &[Kline]) {
        for kline in klines {
            self.data
                .insert(kline.time, VolumePoint::from_volume(kline.volume));
        }
        self.clear_all_caches();
    }

    fn on_insert_trades(
        &mut self,
        _trades: &[Trade],
        old_dp_len: usize,
        source: &PlotData<KlineDataPoint>,
    ) {
        match source {
            PlotData::TimeBased(_) => return,
            PlotData::TickBased(tickseries) => {
                let start_idx = old_dp_len.saturating_sub(1);
                for (idx, dp) in tickseries.datapoints.iter().enumerate().skip(start_idx) {
                    self.data
                        .insert(idx as u64, VolumePoint::from_volume(dp.kline.volume));
                }
            }
        }
        self.clear_all_caches();
    }

    fn on_ticksize_change(&mut self, source: &PlotData<KlineDataPoint>) {
        self.rebuild_from_source(source);
    }

    fn on_basis_change(&mut self, source: &PlotData<KlineDataPoint>) {
        self.rebuild_from_source(source);
    }
}

#[cfg(test)]
mod tests {
    use super::VolumePoint;
    use data::chart::kline::{GroupedTrades, KlineDataPoint, KlineTrades};
    use exchange::{
        Kline, Volume,
        unit::{Price, Qty},
    };
    use rustc_hash::FxHashMap;

    #[test]
    fn volume_point_prefers_footprint_buy_sell_over_total_only_kline_volume() {
        let buy = Qty::from_f32(300.0);
        let sell = Qty::from_f32(100.0);
        let mut trades = FxHashMap::default();
        trades.insert(
            Price::from_f32(10.0),
            GroupedTrades {
                buy_qty: buy,
                sell_qty: sell,
                ..Default::default()
            },
        );

        let point = VolumePoint::from_kline_dp(&KlineDataPoint {
            kline: Kline {
                time: 0,
                open: Price::from_f32(10.0),
                high: Price::from_f32(10.0),
                low: Price::from_f32(10.0),
                close: Price::from_f32(10.0),
                volume: Volume::TotalOnly(Qty::from_f32(999.0)),
            },
            footprint: KlineTrades { trades, poc: None },
        });

        assert_eq!(point.buy_sell, Some((buy, sell)));
        assert_eq!(point.total, buy + sell);
    }

    #[test]
    fn volume_point_falls_back_to_kline_volume_without_footprint_trades() {
        let buy = Qty::from_f32(120.0);
        let sell = Qty::from_f32(80.0);
        let point = VolumePoint::from_kline_dp(&KlineDataPoint {
            kline: Kline {
                time: 0,
                open: Price::from_f32(10.0),
                high: Price::from_f32(10.0),
                low: Price::from_f32(10.0),
                close: Price::from_f32(10.0),
                volume: Volume::BuySell(buy, sell),
            },
            footprint: KlineTrades::new(),
        });

        assert_eq!(point.buy_sell, Some((buy, sell)));
        assert_eq!(point.total, buy + sell);
    }
}
